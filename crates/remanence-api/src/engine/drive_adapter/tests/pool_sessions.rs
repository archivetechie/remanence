//! Session assertions migrated from the deleted direct writer. Every write here
//! uses the owned actor; raw faults are confined to the virtual transport.
use super::*;
use crate::pool_write::*;
use remanence_parity::{
    CommittedBundle, CommittedBundleKind, ParityScheme, SchemeId, TapeFileEntry, TapeFileKind,
    TapeReportPublisher, TapeRowAuthority,
};
use remanence_state::{TapeAuthority, TerminalFinalizationProgress};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

fn scheme(stripe: u16) -> crate::ParityConfig {
    crate::ParityConfig::Scheme(ParityScheme {
        id: SchemeId::new_static("pool-session"),
        data_blocks_per_stripe: stripe,
        parity_blocks_per_stripe: 1,
        stripes_per_neighborhood: 1,
    })
}
fn checkpoint(f: &Fixture) -> TapeAuthority {
    remanence_state::authority::fixtures::AuthorityBuilder::new(
        f.config.journal.dir.join("checkpoints"),
        UUID,
    )
    .build()
    .unwrap()
}
fn parity(f: &Fixture) -> TapeRowAuthority {
    let tape = f.index().get_tape(&UUID).unwrap().unwrap();
    let (_, crate::ParityConfig::Scheme(scheme)) =
        selected_tape_geometry(&tape, "default").unwrap()
    else {
        panic!("parity tape")
    };
    remanence_parity::authority::fixtures::RowAuthorityBuilder::new(
        &f.config.journal.dir,
        UUID,
        BLOCK,
        scheme,
    )
    .build()
    .unwrap()
}
fn command_count(f: &Fixture) -> usize {
    f.commands.lock().unwrap().len()
}
/// Count both LOCATE encodings at the transport boundary, including read positioning.
fn locate_count(f: &Fixture, from: usize) -> usize {
    f.commands.lock().unwrap()[from..]
        .iter()
        .filter(|c| matches!(c[0], 0x2b | 0x92))
        .count()
}
fn no_modifications(f: &Fixture, from: usize) {
    assert!(!f.commands.lock().unwrap()[from..]
        .iter()
        .any(|c| matches!(c[0], 0x0a | 0x10 | 0x19)));
}
fn recipient(byte: u8, slot: u8) -> remanence_aead::RecipientPublicKey {
    remanence_aead::RecipientPrivateKey::new([byte; 16], "test", [byte; 32])
        .unwrap()
        .public_key(slot)
        .unwrap()
}

/// Faults change physical responses only, never catalog or actor outcomes.
struct FaultTransport {
    inner: Transport,
    replica_attempts: Option<Arc<AtomicUsize>>,
    misdirect_locate: bool,
    located: bool,
}
impl SgTransport for FaultTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        bytes: &mut [u8],
    ) -> std::result::Result<TransferOutcome, ScsiError> {
        let result = self.inner.execute_in(cdb, bytes)?;
        if cdb[0] == 0x34 && self.misdirect_locate && self.located {
            bytes[8..16].copy_from_slice(&99u64.to_be_bytes());
        }
        Ok(result)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        bytes: &[u8],
    ) -> std::result::Result<TransferOutcome, ScsiError> {
        if cdb[0] == 0x0a && remanence_parity::parse_tape_index_replica_header(bytes, &UUID).is_ok()
        {
            if let Some(attempts) = &self.replica_attempts {
                attempts.fetch_add(1, Ordering::SeqCst);
                return Err(ScsiError::TransportError {
                    status: 0,
                    host_status: 3,
                    driver_status: 0,
                    info: 1,
                    sense: vec![],
                });
            }
        }
        self.inner.execute_out(cdb, bytes)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> std::result::Result<(), ScsiError> {
        let result = self.inner.execute_none(cdb);
        if matches!(cdb[0], 0x2b | 0x92) {
            self.located = true;
        }
        result
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}
fn configured_owner(
    f: &Fixture,
    mut state: StateHandle,
    memory: Option<u64>,
    replicas: Option<Arc<AtomicUsize>>,
    misdirect: bool,
    admissions: crate::write_admission::WriteAdmissionCoordinator,
) -> MountedDrive {
    let mut mount = crate::OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(FaultTransport {
            inner: Transport {
                inner: ModelTransport::new(f.world.clone(), DeviceRole::Drive { bay: BAY }),
                commands: f.commands.clone(),
                fault: f.fault.clone(),
                evidence: Some(f.config.audit.dir.clone()),
                last_opcode: None,
            },
            replica_attempts: replicas,
            misdirect_locate: misdirect,
            located: false,
        }),
        &mut state,
        f.root.path(),
    )
    .unwrap();
    use crate::MountBackend;
    let mounted = mount
        .mount(crate::MountTarget::Device {
            path: DEVICE.into(),
            expected_uuid: Some(UUID),
        })
        .unwrap();
    mount.finish().unwrap();
    let mut cfg = mounted_config(
        &state,
        &mounted,
        StaticAllowlist::new([SERIAL]),
        Default::default(),
    )
    .unwrap();
    cfg.write_admissions = admissions;
    if let Some(memory) = memory {
        cfg.io_memory = crate::io_memory::IoMemoryReservation::new(memory).unwrap();
    }
    MountedDrive::start_configured(state, mounted, cfg, None).unwrap()
}

/// Fail both the real checkpoint frame fsync and its rollback fsync, then
/// retry through a separate actor so a failed-session gate cannot mask quarantine.
#[test]
fn uncertain_checkpoint_append_quarantines_identity_until_restart() {
    use remanence_chaos::file_model::{FileEvent, MemoryDirectory};
    use remanence_library::durable_file::test_support::register;

    let admissions = crate::write_admission::WriteAdmissionCoordinator::default();
    let (f, state) = Fixture::new(false);
    let files = MemoryDirectory::default();
    let registration = register(
        f.config.journal.dir.join("checkpoints"),
        Arc::new(files.clone()),
    );
    let mut actor = configured_owner(&f, state, None, None, false, admissions.clone());
    actor.open_session("default").unwrap();
    let publication = remanence_state::authority::fixtures::publication_target(&checkpoint(&f));
    let failures = Arc::new(AtomicUsize::new(0));
    let observed = failures.clone();
    let backend = files.clone();
    files.observe(Some(Arc::new(move |event| {
        if let FileEvent::BeforeSync(path) = event {
            if publication.matches(&path) && observed.load(Ordering::SeqCst) < 2 {
                backend.fail_next_sync(&path).unwrap();
                observed.fetch_add(1, Ordering::SeqCst);
            }
        }
    })));
    let error = actor.append(f.request("uncertain-caller")).unwrap_err();
    assert_eq!(
        failures.load(Ordering::SeqCst),
        2,
        "frame and rollback fsync failed"
    );
    assert!(
        error
            .status
            .message()
            .contains("durable authority uncertain"),
        "{error}"
    );
    assert!(
        error.status.message().contains("rollback to offset"),
        "{error}"
    );
    assert!(
        error.status.message().contains("journal reconciliation"),
        "{error}"
    );
    assert!(f
        .index()
        .get_native_object_by_unique_caller_object_id("uncertain-caller")
        .unwrap()
        .is_none());
    files.observe(None);
    let _ = actor.finish();
    drop(registration);

    // Another drive has no tape fence or failed-session latch. Only the shared
    // in-process identity claim can refuse its otherwise valid append.
    let (other, state) = Fixture::new(false);
    let mut retry = configured_owner(&other, state, None, None, false, admissions.clone());
    retry.open_session("default").unwrap();
    let before = command_count(&other);
    let error = retry.append(other.request("uncertain-caller")).unwrap_err();
    assert_eq!(error.status.code(), tonic::Code::Aborted, "{error}");
    assert!(
        error
            .status
            .message()
            .contains("same pool/caller replay key"),
        "{error}"
    );
    no_modifications(&other, before);
    retry.finish().unwrap();
    // Recreating the process-wide coordinator models restart. It does not
    // release the first tape's durable fence or bypass journal reconciliation.
    drop(admissions);
    let mut restarted = configured_owner(
        &other,
        other.reopen().unwrap(),
        None,
        None,
        false,
        Default::default(),
    );
    let committed = restarted
        .write_checkpointed(other.request("uncertain-caller"))
        .unwrap();
    assert_eq!(committed.object.caller_object_id, "uncertain-caller");
    restarted.finish().unwrap();
}

#[test]
fn selected_drive_writer_binds_media_check_and_write_to_one_drive() {
    let (f, state) = Fixture::new(false);
    f.world.lock().unwrap().tapes.get_mut("HAND").unwrap().worm = true;
    let (library, drive, policy) = super::library::library_parts(&f);
    let before = command_count(&f);
    let mut actor = MountedDrive::from_placed_library(
        state,
        library,
        drive,
        policy,
        UUID,
        "worm",
        f.root.path(),
    )
    .unwrap();
    let error = actor.write_checkpointed(f.request("worm")).unwrap_err();
    assert!(error.to_string().to_lowercase().contains("worm"), "{error}");
    let _ = actor.finish();
    let commands = f.commands.lock().unwrap();
    let opcodes: Vec<_> = commands[before..].iter().map(|c| c[0]).collect();
    assert!(opcodes.contains(&0x01) && opcodes.contains(&0x08));
    assert!(opcodes.contains(&0x1a));
    for forbidden in [0x15, 0x0a, 0x10] {
        assert!(!opcodes.contains(&forbidden));
    }
    drop(commands);
    f.world.lock().unwrap().tapes.get_mut("HAND").unwrap().worm = false;
    let before = command_count(&f);
    let mut actor = super::library::library_owner(&f, f.reopen().unwrap(), "rewritable").unwrap();
    let result = actor.write_checkpointed(f.request("rewritable")).unwrap();
    assert_eq!(result.object.copies.len(), 1);
    let commands = f.commands.lock().unwrap();
    let opcodes: Vec<_> = commands[before..].iter().map(|c| c[0]).collect();
    let mode_sense = opcodes.iter().position(|c| *c == 0x1a).unwrap();
    let mode_select = opcodes.iter().position(|c| *c == 0x15).unwrap();
    let verified = opcodes
        .iter()
        .enumerate()
        .find(|(i, c)| *i > mode_select && **c == 0x1a)
        .unwrap()
        .0;
    let write = opcodes
        .iter()
        .position(|c| matches!(c, 0x0a | 0x10))
        .unwrap();
    assert!(mode_sense < mode_select && mode_select < verified && verified < write);
    let before = commands.len();
    drop(commands);
    let replay = actor.write_checkpointed(f.request("rewritable")).unwrap();
    assert!(replay.is_replay());
    assert_eq!(replay.object.object_id, result.object.object_id);
    assert_eq!(command_count(&f), before);
    let conflict = f.request("rewritable");
    std::fs::write(
        f.root.path().join("rewritable"),
        b"different caller content",
    )
    .unwrap();
    assert!(
        matches!(actor.write_checkpointed(conflict), Err(crate::MountError::PoolWrite(cause)) if matches!(*cause, PoolWriteError::CallerObjectIdConflict { .. }))
    );
    assert_eq!(command_count(&f), before);
    actor.finish().unwrap();
}

#[test]
fn encrypted_parity_write_uses_configured_spool_ceiling_before_object_motion() {
    let (f, state) = Fixture::with_parity(scheme(2));
    let mut actor = configured_owner(&f, state, Some(1), None, false, Default::default());
    let mut input = f.request("encrypted-capacity");
    input.representation = PoolWriteRepresentation::Encrypted {
        recipients: vec![recipient(0x71, 0), recipient(0x72, 1)],
    };
    let before = command_count(&f);
    let error = actor.write_checkpointed(input).unwrap_err();
    assert!(
        error.to_string().to_lowercase().contains("spool"),
        "{error}"
    );
    if let crate::MountError::Actor(failure) = &error {
        assert!(
            matches!(
                failure.pool_write_error.as_deref(),
                Some(PoolWriteError::ParitySpoolRefused { cause, .. }) if matches!(cause.as_ref(), remanence_parity::ParityError::CapacityReserveExceeded { cause: remanence_parity::CapacityReserveCause::ParitySpoolCapacity, remaining_spool_bytes: Some(1), .. })
            ),
            "{failure:?}"
        );
    } else {
        panic!("typed capacity error: {error:?}");
    }
    let writes = f.commands.lock().unwrap()[before..]
        .iter()
        .filter(|c| c[0] == 0x0a)
        .count();
    assert_eq!(writes, 1, "only the identity bootstrap is written");
    assert_eq!(
        f.world.lock().unwrap().tapes["HAND"].records.len(),
        2,
        "only the original identity bootstrap and delimiter"
    );
    assert!(f
        .index()
        .get_native_object_by_unique_caller_object_id("encrypted-capacity")
        .unwrap()
        .is_none());
    let _ = actor.finish();
    assert!(checkpoint(&f).replay().unwrap().is_empty());
}

#[test]
fn checkpointed_parity_raw_write_failure_persists_fence_and_blocks_retry() {
    let (f, state) = Fixture::with_parity(scheme(2));
    let mut actor = f.owner(state);
    actor.open_session("default").unwrap();
    *f.fault.lock().unwrap() = Some(0x0a);
    assert!(actor.write_checkpointed(f.request("raw-fence")).is_err());
    let _ = actor.finish();
    assert_eq!(f.world.lock().unwrap().tapes["HAND"].records.len(), 2);
    assert!(checkpoint(&f).replay().unwrap().is_empty());
    let index = f.index();
    let fences = index
        .tape_io_admission_conflicts(&UUID, Some("HAND"))
        .unwrap();
    assert_eq!(
        fences
            .iter()
            .filter(|f| f.reason == "transfer_error")
            .count(),
        1
    );
    assert_eq!(fences.len(), 2);
    assert!(fences.iter().any(|f| f.reason == "standalone_write_failed"));
    crate::tests::assert_fences_survive_empty_rebuild(&index, &f.config.audit.dir);
    let before = command_count(&f);
    let mut retry = f.owner(f.reopen().unwrap());
    assert!(retry.write_checkpointed(f.request("raw-retry")).is_err());
    let _ = retry.finish();
    no_modifications(&f, before);
}

#[test]
fn batch_of_one_core_journals_tape_for_later_daemon_admission() {
    let (f, state) = Fixture::new(false);
    let journal = checkpoint(&f);
    let lease = journal.acquire_exclusive().unwrap();
    let mut actor = f.owner(state);
    let before = command_count(&f);
    assert!(actor.write_checkpointed(f.request("leased")).is_err());
    assert_eq!(
        command_count(&f),
        before,
        "competing checkpoint lease rejects before positioning"
    );
    drop(lease);
    let result = actor.write_checkpointed(f.request("batch-one")).unwrap();
    actor.finish().unwrap();
    let record = journal.last().unwrap().unwrap();
    assert_eq!(record.committed_object_count, 1);
    assert_eq!(record.objects.len(), 1);
    let index = f.index();
    assert!(index
        .get_native_object(&uuid::Uuid::from_bytes(result.object.object_id).to_string())
        .unwrap()
        .is_some());
    assert_eq!(
        select_tape_in_pool_for_write_session(
            &index,
            &f.config.tape_pools[0],
            7,
            &HashSet::new(),
            &f.config.journal.dir.join("checkpoints")
        )
        .unwrap()
        .tape_uuid,
        UUID
    );
}

#[test]
fn watermark_seal_journals_exact_terminal_authority_and_host_suffix() {
    for parity_enabled in [false, true] {
        let (mut f, state) = Fixture::new(parity_enabled);
        drop(state);
        f.config.tape_pools[0].capacity_cap_bytes = Some(3 * 1024 * 1024 * 1024);
        f.config.tape_pools[0].watermark_low = 0.000001;
        f.config.tape_pools[0].watermark_high = 0.25;
        let mut actor = f.owner(f.reopen().unwrap());
        actor
            .write_checkpointed(f.request("terminal-success"))
            .unwrap();
        actor.finish().unwrap();
        let records = checkpoint(&f).replay().unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].objects.len(), 1);
        assert!(!records[0].sealed_after_write);
        let terminal = &records[1];
        assert!(terminal.objects.is_empty());
        assert!(terminal.sealed_after_write);
        assert_eq!(terminal.committed_object_count, 1);
        assert_eq!(terminal.ordinal, records[0].ordinal + 1);
        assert_eq!(
            terminal.barrier_bundle.as_ref().map(|b| b.kind),
            Some(CommittedBundleKind::TerminalComponent)
        );
        let finalization = terminal.terminal_finalization.as_ref().unwrap();
        assert_eq!(
            finalization.progress,
            TerminalFinalizationProgress::AfterReplicaC
        );
        assert_eq!(finalization.layout.components[4].ordinal, 3);
        assert_eq!(
            terminal.next_tape_file_number,
            finalization.layout.components[4].tape_file_number + 1
        );
        let physical_eod = f.world.lock().unwrap().tapes["HAND"].records.len() as u64;
        assert_eq!(terminal.eod_partition, 0);
        assert_eq!(terminal.eod_lba, physical_eod);
        let tape = f.index().get_tape(&UUID).unwrap().unwrap();
        assert_eq!(tape.state, "sealed");
        assert_eq!(tape.written_extent_lba, Some(physical_eod));
        if parity_enabled {
            assert!(finalization.terminal_prefix.is_some());
            let (_, crate::ParityConfig::Scheme(scheme)) =
                selected_tape_geometry(&tape, "default").unwrap()
            else {
                panic!()
            };
            assert_eq!(terminal.scheme.as_ref(), Some(&scheme));
            let committed = parity(&f).load_committed().unwrap();
            assert!(committed.orphaned_bundles.is_empty());
            assert_eq!(
                committed.entries.last().map(|e| e.kind),
                Some(TapeFileKind::TapeIndexReplica)
            );
        }
        // Re-enter the actor's host-only terminal completion twice: idempotent
        // sealing and the StateHandle audit append cursor must remain intact.
        let mut actor = f.owner(f.reopen().unwrap());
        for _ in 0..2 {
            assert!(actor.open_session("default").is_err());
        }
        actor.finish().unwrap();
        let mut state = f.reopen().unwrap();
        let session_id = uuid::Uuid::from_u128(0x9901);
        state
            .audit()
            .append(remanence_state::AuditEventRecord {
                actor: remanence_state::AuditActor::System,
                source_layer: remanence_state::SourceLayer::Layer5,
                operation_id: None,
                session_id: Some(session_id),
                idempotency_key: None,
                event: AuditEvent::SessionOpened,
                subject: remanence_state::AuditSubject {
                    kind: "write".into(),
                    id: Some(session_id.to_string()),
                },
                detail: std::collections::BTreeMap::from([(
                    "session_kind".into(),
                    ciborium::Value::Text("write".into()),
                )]),
            })
            .unwrap();
        let audit = FileAuditLog::replay(&f.config.audit.dir).unwrap();
        assert_eq!(
            audit
                .iter()
                .filter(|r| r.event == AuditEvent::TapeSealed
                    && crate::audit_projection::audit_subject_matches_tape(r, UUID))
                .count(),
            1
        );
        assert!(audit.windows(2).all(|p| p[1].sequence == p[0].sequence + 1));
        assert_eq!(
            audit.last().map(|r| &r.event),
            Some(&AuditEvent::SessionOpened)
        );
    }
}

#[test]
fn terminal_failure_preserves_object_checkpoint_and_structured_intent() {
    let (mut f, state) = Fixture::new(false);
    drop(state);
    f.config.tape_pools[0].capacity_cap_bytes = Some(3 * 1024 * 1024 * 1024);
    f.config.tape_pools[0].watermark_low = 0.000001;
    f.config.tape_pools[0].watermark_high = 0.25;
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut actor = configured_owner(
        &f,
        f.reopen().unwrap(),
        None,
        Some(attempts.clone()),
        false,
        Default::default(),
    );
    let error = actor
        .write_checkpointed(f.request("terminal-failure"))
        .unwrap_err();
    assert!(
        matches!(error, crate::MountError::Committed { .. }),
        "{error}"
    );
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let _ = actor.finish();
    let index = f.index();
    crate::tests::assert_fences_survive_empty_rebuild(&index, &f.config.audit.dir);
    let journal = checkpoint(&f);
    let error = journal.replay().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("pending terminal finalization intent"),
        "{error}"
    );
    let recovery = journal
        .acquire_exclusive_for_terminal_recovery()
        .and_then(|mut l| l.replay_for_terminal_recovery())
        .unwrap();
    assert_eq!(recovery.records.len(), 1);
    assert_eq!(recovery.records[0].objects.len(), 1);
    let intent = recovery.finalization_intent.unwrap();
    assert_eq!(
        intent.progress,
        TerminalFinalizationProgress::BeforeReplicaA
    );
    assert!(intent.recovery_required);
    assert_eq!(
        index.get_tape(&UUID).unwrap().unwrap().state,
        "recovery_required"
    );
    let fences = index
        .tape_io_admission_conflicts(&UUID, Some("HAND"))
        .unwrap();
    assert_eq!(fences.len(), 1);
    assert_eq!(fences[0].reason, "transfer_error");
    assert!(index
        .get_native_object_by_unique_caller_object_id("terminal-failure")
        .unwrap()
        .is_some());
}

#[test]
fn fresh_parity_requires_exact_bot_position_before_bootstrap_write() {
    let (f, state) = Fixture::new(true);
    let mut actor = configured_owner(&f, state, None, None, true, Default::default());
    let before = command_count(&f);
    let records_before = f.world.lock().unwrap().tapes["HAND"].records.clone();
    let error = actor
        .write_checkpointed(f.request("bot-proof"))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("expected TapePosition { lba: 0, partition: 0,")
            && error.to_string().contains("99"),
        "{error}"
    );
    let _ = actor.finish();
    no_modifications(&f, before);
    assert_eq!(
        f.world.lock().unwrap().tapes["HAND"].records,
        records_before,
        "neither a block nor a filemark was emitted"
    );
    assert_eq!(
        f.world.lock().unwrap().tapes["HAND"].written_bytes,
        u64::from(BLOCK)
    );
    assert_eq!(
        locate_count(&f, before),
        3,
        "two BOT identity probes and one append-position restoration"
    );
    assert!(checkpoint(&f).replay().unwrap().is_empty());
    let committed = parity(&f).load_committed().unwrap();
    assert!(committed.entries.is_empty());
    assert!(committed.orphaned_bundles.is_empty());
    let tape = f.index().get_tape(&UUID).unwrap().unwrap();
    assert_eq!(tape.last_committed_tape_file, None);
    assert_eq!(tape.written_extent_lba, None);
}

/// Zero protection watermark still belongs to a parity writer.
#[test]
fn encrypted_first_epoch_journals_parity_state_and_replays() {
    for (stripe, size) in [(128, 32 * 1024), (2, 768 * 1024)] {
        let (f, state) = Fixture::with_parity(scheme(stripe));
        let mut actor = f.owner(state);
        let mut input = f.request("encrypted-first");
        std::fs::write(f.root.path().join("encrypted-first"), vec![0x5a; size]).unwrap();
        input.representation = PoolWriteRepresentation::Encrypted {
            recipients: vec![recipient(0x71, 0), recipient(0x72, 1)],
        };
        let result = actor.write_checkpointed(input).unwrap();
        actor.close_session().unwrap();
        // A checkpoint leaves a partial epoch open. Finish closes it separately.
        let records = checkpoint(&f).replay().unwrap();
        assert_eq!(records.len(), 1);
        let bundle = &records[0].object_tape_file_bundles[0];
        if stripe == 128 {
            assert_eq!(bundle.highest_protected_ordinal, 0);
        } else {
            assert!(bundle.highest_protected_ordinal > 0);
        }
        assert!(records[0].objects[0]
            .copy
            .first_parity_data_ordinal
            .is_some());
        assert!(records[0].scheme.is_some());
        assert!(!records[0].object_tape_file_bundles.is_empty());
        let mut index = f.index();
        let id = uuid::Uuid::from_bytes(result.object.object_id).to_string();
        let expected = index.get_native_object(&id).unwrap().unwrap();
        index.project_checkpoint_record(&records[0]).unwrap();
        assert_eq!(index.get_native_object(&id).unwrap().unwrap(), expected);
        actor.finish().unwrap();
    }
}

#[test]
fn bootstrap_only_parity_orphan_requires_physical_reconciliation_before_positioning() {
    let (f, mut state) = Fixture::with_parity(scheme(8));
    let selected = select_tape_in_pool(
        state.catalog_index(),
        &f.config.tape_pools[0],
        7,
        &HashSet::new(),
    )
    .unwrap();
    let crate::ParityConfig::Scheme(scheme) = selected.parity_config.clone() else {
        panic!()
    };
    let mut journal = parity(&f);
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![(CommittedBundle {
                kind: CommittedBundleKind::BotBootstrap,
                entries: vec![TapeFileEntry {
                    tape_file_number: 0,
                    kind: TapeFileKind::Bootstrap,
                    block_count: 1,
                    physical_start_hint: Some(0),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    object_recovery_row: None,
                }],
                highest_protected_ordinal: 0,
                total_committed_ordinals: 0,
            })
            .clone()],
        })
        .unwrap();
    project_fresh_parity_bootstrap_bundle(state.catalog_index(), &selected, &scheme).unwrap();
    let committed = journal.load_committed().unwrap();
    assert!(committed.entries.is_empty());
    assert_eq!(committed.orphaned_bundles.len(), 1);
    let tape = state.catalog_index().get_tape(&UUID).unwrap().unwrap();
    assert_eq!(tape.total_committed_ordinals, 0);
    assert_eq!(tape.last_committed_tape_file, Some(0));
    let checkpoint_dir = f.config.journal.dir.join("checkpoints");
    assert_eq!(
        select_tape_in_pool_for_write_session(
            state.catalog_index(),
            &f.config.tape_pools[0],
            7,
            &HashSet::new(),
            &checkpoint_dir
        )
        .unwrap()
        .tape_uuid,
        UUID
    );
    assert!(matches!(admit_pinned_tape_for_write_session(
        state.catalog_index(), UUID, "default", &f.config.tape_pools[0], &checkpoint_dir
    ).unwrap(), PinnedWriteDisposition::Writable(selected) if selected.tape_uuid == UUID));
    drop(journal);
    let mut actor = f.owner(state);
    let before = command_count(&f);
    let error = actor.write_checkpointed(f.request("orphan")).unwrap_err();
    assert!(
        error.to_string().contains("journal recovery required"),
        "{error}"
    );
    no_modifications(&f, before);
    let _ = actor.finish();
    let preserved = parity(&f).load_committed().unwrap();
    assert!(preserved.entries.is_empty());
    assert_eq!(preserved.orphaned_bundles.len(), 1);
}

#[test]
fn checkpointed_parity_tape_is_admitted_for_daemon_resume() {
    let (f, state) = Fixture::with_parity(scheme(8));
    let mut actor = f.owner(state);
    let fresh = command_count(&f);
    actor.write_checkpointed(f.request("resume-first")).unwrap();
    assert_eq!(
        locate_count(&f, fresh),
        3,
        "two BOT identity probes and one append-position restoration"
    );
    let last_read = {
        let commands = f.commands.lock().unwrap();
        fresh
            + commands[fresh..]
                .iter()
                .rposition(|c| c[0] == 0x08)
                .expect("physical suffix was read")
            + 1
    };
    assert_eq!(
        locate_count(&f, last_read),
        1,
        "fresh append positions once after the read-only proof"
    );
    actor.close_session().unwrap();
    let before = command_count(&f);
    let index = f.index();
    let checkpoint_dir = f.config.journal.dir.join("checkpoints");
    let cfg = &f.config.tape_pools[0];
    assert_eq!(
        select_tape_in_pool_for_write_session(&index, cfg, 7, &HashSet::new(), &checkpoint_dir)
            .unwrap()
            .tape_uuid,
        UUID
    );
    assert!(
        matches!(admit_pinned_tape_for_write_session(&index, UUID, "default", cfg, &checkpoint_dir).unwrap(), PinnedWriteDisposition::Writable(selected) if selected.tape_uuid == UUID)
    );
    let mut journal = parity(&f);
    let committed = journal.load_committed().unwrap();
    let next_file = committed.entries.last().unwrap().tape_file_number + 1;
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![(CommittedBundle {
                kind: CommittedBundleKind::Object,
                entries: vec![TapeFileEntry {
                    tape_file_number: next_file,
                    kind: TapeFileKind::Object,
                    block_count: 1,
                    physical_start_hint: Some(100),
                    object_id: Some("sink-ahead".into()),
                    first_parity_data_ordinal: Some(committed.total_committed_ordinals),
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    object_recovery_row: None,
                }],
                highest_protected_ordinal: committed.highest_protected_ordinal,
                total_committed_ordinals: committed.total_committed_ordinals + 1,
            })
            .clone()],
        })
        .unwrap();
    journal
        .publish_fixture(&remanence_parity::authority::ShadowPublication {
            bundles: vec![(CommittedBundle {
                kind: CommittedBundleKind::CheckpointedThrough,
                entries: vec![],
                highest_protected_ordinal: committed.highest_protected_ordinal,
                total_committed_ordinals: committed.total_committed_ordinals + 1,
            })
            .clone()],
        })
        .unwrap();
    assert!(journal
        .load_committed()
        .unwrap()
        .orphaned_bundles
        .is_empty());
    drop(journal);
    let error = actor
        .write_checkpointed(f.request("resume-conflict"))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("bounded parity terminal authority mismatch"),
        "{error}"
    );
    no_modifications(&f, before);
    let sink_ahead_locates = locate_count(&f, before);
    let path =
        remanence_state::authority::fixtures::structural_location(&checkpoint_dir, UUID).unwrap();
    let _hidden = remanence_parity::authority::fixtures::hide_authority(&path).unwrap();
    let missing_before = command_count(&f);
    let error = actor
        .write_checkpointed(f.request("resume-missing"))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("bounded parity terminal authority mismatch"),
        "{error}"
    );
    no_modifications(&f, before);
    let missing_locates = locate_count(&f, missing_before);
    // The deleted direct writer refused both cases before any LOCATE. The drive actor refuses them after its two
    // read-only BOT identity probes and the append-position LOCATE, still before any modifying command. Validating
    // parity authority before positioning is deferred to the actor rework; this pins today's count so that change
    // is deliberate.
    assert_eq!(
        (sink_ahead_locates, missing_locates),
        (3, 3),
        "both authority refusals follow only the identity probes and the append-position LOCATE"
    );
    let _ = actor.finish();
}
