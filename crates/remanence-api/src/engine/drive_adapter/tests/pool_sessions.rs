//! Session assertions migrated from the deleted direct writer. Every write here
//! uses the owned actor; raw faults are confined to the virtual transport.
use super::*;
use crate::pool_write::*;
use remanence_parity::{ParityScheme, SchemeId, TapeFileKind, TapeReportPublisher};
use remanence_state::authority::row_fixtures::RowAuthority as TapeRowAuthority;
use remanence_state::authority::TapeAuthorityView;
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
        f.config.journal.dir.join("tapes"),
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
    remanence_state::authority::row_fixtures::RowAuthorityBuilder::new(
        f.config.journal.dir.join("tapes"),
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
    state: StateHandle,
    memory: Option<u64>,
    replicas: Option<Arc<AtomicUsize>>,
    misdirect: bool,
    admissions: crate::write_admission::WriteAdmissionCoordinator,
) -> MountedDrive {
    configured_owner_and_memory(f, state, memory, replicas, misdirect, admissions).0
}

fn configured_owner_and_memory(
    f: &Fixture,
    mut state: StateHandle,
    memory: Option<u64>,
    replicas: Option<Arc<AtomicUsize>>,
    misdirect: bool,
    admissions: crate::write_admission::WriteAdmissionCoordinator,
) -> (MountedDrive, Arc<crate::io_memory::IoMemoryReservation>) {
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
    let memory = cfg.io_memory.clone();
    (
        MountedDrive::start_configured(state, mounted, cfg, None).unwrap(),
        memory,
    )
}

/// Fail both the real checkpoint frame fsync and its rollback fsync, then
/// retry through a separate actor so a failed-session gate cannot mask quarantine.
#[test]
fn uncertain_checkpoint_append_quarantines_identity_until_restart() {
    use remanence_chaos::file_model::{FileEvent, MemoryDirectory};
    use remanence_library::durable_file::test_support::register;

    for parity_enabled in [false, true] {
        let admissions = crate::write_admission::WriteAdmissionCoordinator::default();
        let (f, state) = Fixture::new(parity_enabled);
        let files = MemoryDirectory::default();
        let registration = register(f.config.journal.dir.join("tapes"), Arc::new(files.clone()));
        let mut actor = configured_owner(&f, state, None, None, false, admissions.clone());
        actor.open_session("default").unwrap();
        let publication = remanence_state::authority::fixtures::publication_target(&checkpoint(&f));
        let failures = Arc::new(AtomicUsize::new(0));
        let observed = failures.clone();
        let backend = files.clone();
        let failed_at = Arc::new(AtomicUsize::new(usize::MAX));
        let observed_at = failed_at.clone();
        let commands = f.commands.clone();
        let syncs = AtomicUsize::new(0);
        files.observe(Some(Arc::new(move |event| {
            if let FileEvent::BeforeSync(path) = event {
                if publication.matches(&path) && syncs.fetch_add(1, Ordering::SeqCst) == 1 {
                    observed_at.store(commands.lock().unwrap().len(), Ordering::SeqCst);
                    backend.fail_next_sync(&path).unwrap();
                    observed.fetch_add(1, Ordering::SeqCst);
                }
            }
        })));
        let error = actor.append(f.request("uncertain-caller")).unwrap_err();
        assert_eq!(
            failures.load(Ordering::SeqCst),
            1,
            "single tape-log fsync failed"
        );
        assert!(
            error
                .status
                .message()
                .contains("durable authority uncertain"),
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
        let before_retry = command_count(&f);
        assert!(actor
            .append(f.request("after-publication-failure"))
            .is_err());
        no_modifications(&f, before_retry);
        files.observe(None);
        let _ = actor.finish();
        no_modifications(&f, failed_at.load(Ordering::SeqCst));
        assert!(f
            .index()
            .tape_io_is_fenced(Some(&UUID), None, None)
            .unwrap());
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
    assert!(error.to_string().contains("CloseoutMemory:"), "{error}");
    if let crate::MountError::Actor(failure) = &error {
        assert_eq!(failure.status.code(), tonic::Code::ResourceExhausted);
    } else {
        panic!("typed capacity error: {error:?}");
    }
    let writes = f.commands.lock().unwrap()[before..]
        .iter()
        .filter(|c| c[0] == 0x0a)
        .count();
    assert_eq!(writes, 0, "reservation refuses before any drive command");
    assert_eq!(command_count(&f), before);
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
fn checkpointed_parity_raw_write_failure_names_tail_and_allows_retry() {
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
    assert!(
        fences.is_empty(),
        "durable live ends do not fence reads or retries"
    );
    let mut retry = f.owner(f.reopen().unwrap());
    retry.write_checkpointed(f.request("raw-retry")).unwrap();
    retry.finish().unwrap();
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
            &f.config.journal.dir.join("tapes")
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
        assert_eq!(terminal.barrier_bundle.as_ref().map(|b| b.kind), None);
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
            let authority = checkpoint(&f).freeze().unwrap();
            let mut last = None;
            remanence_state::authority::TapeAuthoritySnapshot::visit_file_rows(
                &authority,
                &mut |row| {
                    last = Some(row.kind);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(last, Some(TapeFileKind::TapeIndexReplica));
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
    let receipt = actor
        .write_checkpointed(f.request("terminal-failure"))
        .unwrap();
    assert!(
        !receipt.object.object_id.is_empty(),
        "object receipt precedes terminal work"
    );
    let _ = actor.finish();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    let index = f.index();
    crate::tests::assert_fences_survive_empty_rebuild(&index, &f.config.audit.dir);
    let journal = checkpoint(&f);
    assert_eq!(journal.replay().unwrap().len(), 1);
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
        index
            .project_tape_log(
                &remanence_state::authority::TapeAuthorityView::freeze(&checkpoint(&f)).unwrap(),
            )
            .unwrap();
        assert_eq!(index.get_native_object(&id).unwrap().unwrap(), expected);
        actor.finish().unwrap();
    }
}

#[test]
fn bootstrap_open_commits_bot_before_objects_in_both_parity_modes() {
    for parity_enabled in [false, true] {
        let (f, state) = if parity_enabled {
            Fixture::with_parity(scheme(8))
        } else {
            Fixture::new(false)
        };
        let mut actor = f.owner(state);
        assert!(
            !remanence_state::TapeAuthority::exists(f.config.journal.dir.join("tapes"), UUID)
                .unwrap()
        );
        actor.open_session("default").unwrap();
        assert!(
            remanence_state::TapeAuthority::exists(f.config.journal.dir.join("tapes"), UUID)
                .unwrap()
        );
        let tape = f.index().get_tape(&UUID).unwrap().unwrap();
        assert_eq!(tape.total_committed_ordinals, 0);
        assert_eq!(tape.highest_protected_ordinal, 0);
        assert_eq!(tape.last_committed_tape_file, Some(0));
        let commands = f.commands.lock().unwrap();
        let modifying: Vec<_> = commands
            .iter()
            .filter(|c| matches!(c[0], 0x0a | 0x10))
            .collect();
        assert_eq!(
            modifying.len(),
            3,
            "BOT block, filemark, synchronous barrier"
        );
        assert_eq!(modifying[0][0], 0x0a);
        assert_eq!(modifying[1][0], 0x10);
        assert_eq!(&modifying[2][..5], &[0x10, 0, 0, 0, 0]);
        drop(commands);
        actor.finish().unwrap();
        let summary = checkpoint(&f).authority_summary().unwrap();
        assert_eq!(
            summary.cursor,
            Some(remanence_parity::CommitCursor {
                partition: 0,
                lba: 2
            })
        );
        assert_eq!(summary.checkpoint_ordinal, 0);
        assert_eq!(summary.object_count, 0);

        assert_eq!(f.index().list_tape_files(&UUID).unwrap().len(), 1);
        let before = command_count(&f);
        let mut restarted = f.owner(f.reopen().unwrap());
        restarted.open_session("default").unwrap();
        no_modifications(&f, before);
        restarted
            .write_checkpointed(f.request("after-bootstrap"))
            .unwrap();
        restarted.finish().unwrap();
        assert_eq!(
            checkpoint(&f)
                .replay()
                .unwrap()
                .iter()
                .map(|r| r.objects.len())
                .sum::<usize>(),
            1
        );
    }
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
    let checkpoint_dir = f.config.journal.dir.join("tapes");
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
    let path = checkpoint_dir.join(format!("{}.remtape", uuid::Uuid::from_bytes(UUID).simple()));
    let saved = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(select_tape_in_pool_for_write_session(
        &index,
        cfg,
        7,
        &HashSet::new(),
        &checkpoint_dir
    )
    .is_err());
    assert!(
        admit_pinned_tape_for_write_session(&index, UUID, "default", cfg, &checkpoint_dir).is_err()
    );
    no_modifications(&f, before);
    std::fs::write(&path, saved).unwrap();
    actor
        .write_checkpointed(f.request("resume-second"))
        .unwrap();
    assert!(command_count(&f) > before);
    actor.finish().unwrap();
    assert_eq!(
        checkpoint(&f)
            .replay()
            .unwrap()
            .iter()
            .map(|r| r.objects.len())
            .sum::<usize>(),
        2
    );
}

/// Fail the actual closing-frame sync after each physical barrier. No epoch
/// action injection is involved; the normal actor publication/fence paths run.
#[test]
fn closeout_prefix_and_component_publication_failures_poison_release_and_fence() {
    use remanence_chaos::file_model::{FileEvent, MemoryDirectory};
    use remanence_library::durable_file::test_support::register;
    use remanence_state::tape_log::{codec::Decoder, store::Header, Record};
    for target in ["closeout", "prefix", "component"] {
        let (mut f, state) = Fixture::with_parity(scheme(128));
        drop(state);
        if target != "closeout" {
            f.config.tape_pools[0].capacity_cap_bytes = Some(3 * 1024 * 1024 * 1024);
            f.config.tape_pools[0].watermark_low = 0.000001;
            f.config.tape_pools[0].watermark_high = 0.25;
        }
        let files = MemoryDirectory::default();
        let registration = register(f.config.journal.dir.join("tapes"), Arc::new(files.clone()));
        let (mut actor, memory) = configured_owner_and_memory(
            &f,
            f.reopen().unwrap(),
            None,
            None,
            false,
            Default::default(),
        );
        let failed_at = Arc::new(AtomicUsize::new(usize::MAX));
        let observed = failed_at.clone();
        let commands = f.commands.clone();
        let backend = files.clone();
        files.observe(Some(Arc::new(move |event| {
            if let FileEvent::BeforeSync(path) = event {
                if observed.load(Ordering::SeqCst) != usize::MAX
                    || path.extension().is_none_or(|e| e != "remtape")
                {
                    return;
                }
                let snapshot = backend.snapshot();
                let Some((_, inode)) = snapshot.files().find(|(p, _)| *p == path) else {
                    return;
                };
                let mut reader = std::io::Cursor::new(&inode.written);
                if Header::decode(&mut reader, UUID).is_err() {
                    return;
                }
                let offset = reader.position();
                let mut decoder = Decoder::at(reader, 1, offset);
                let mut last = None;
                while let Ok(Some(frame)) = decoder.next_frame() {
                    last = Some(frame.record);
                }
                let selected = match last {
                    Some(Record::ClosedOut(_)) => target == "closeout",
                    Some(Record::TerminalPrefix { .. }) => target == "prefix",
                    Some(Record::ComponentWritten { .. }) => target == "component",
                    _ => false,
                };
                if selected {
                    observed.store(commands.lock().unwrap().len(), Ordering::SeqCst);
                    backend.fail_next_sync(&path).unwrap();
                }
            }
        })));
        if target == "closeout" {
            actor
                .write_checkpointed(f.request("retained-object"))
                .unwrap();
            assert!(memory.granted() > 0, "session accumulator is reserved");
            let _ = actor.finish();
        } else {
            actor
                .write_checkpointed(f.request("terminal-object"))
                .unwrap();
            let _ = actor.close_session();
            let before_retry = command_count(&f);
            assert!(actor.append(f.request("poisoned-retry")).is_err());
            no_modifications(&f, before_retry);
            let _ = actor.finish();
        }
        let failed = failed_at.load(Ordering::SeqCst);
        assert_ne!(failed, usize::MAX, "{target} reached its closing fsync");
        no_modifications(&f, failed);
        assert_eq!(memory.granted(), 0, "{target} released session memory");
        assert!(
            f.index()
                .tape_io_is_fenced(Some(&UUID), None, None)
                .unwrap(),
            "{target} persisted SQLite fence"
        );
        assert!(
            checkpoint(&f)
                .acquire_exclusive_for_terminal_recovery()
                .is_err(),
            "ambiguous authority refuses further publication"
        );
        files.observe(None);
        drop(registration);
    }
}

/// Kill the actual owner process at either side of the BOT durability barrier.
/// Only the injected virtual transport is used; save its BOT for restart proof.
struct BootstrapCrashTransport {
    inner: ModelTransport,
    world: SharedVirtualWorld,
    manifest: PathBuf,
    after_barrier: bool,
}
impl SgTransport for BootstrapCrashTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        bytes: &mut [u8],
    ) -> std::result::Result<TransferOutcome, ScsiError> {
        self.inner.execute_in(cdb, bytes)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        bytes: &[u8],
    ) -> std::result::Result<TransferOutcome, ScsiError> {
        self.inner.execute_out(cdb, bytes)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> std::result::Result<(), ScsiError> {
        if matches!(cdb[0], 0x1e | 0x16 | 0x17) {
            return Ok(());
        }
        if cdb.starts_with(&[0x10, 0, 0, 0, 0]) {
            if self.after_barrier {
                self.inner.execute_none(cdb)?;
            }
            let world = self.world.lock().unwrap();
            let records = &world.tapes["HAND"].records;
            let [Record::Block(bot), Record::Filemark] = records.as_slice() else {
                panic!("crash cut must contain exactly BOT and its filemark")
            };
            std::fs::write(self.manifest.with_extension("bot"), bot).unwrap();
            std::process::exit(86);
        }
        self.inner.execute_none(cdb)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

#[test]
#[ignore = "subprocess helper for bootstrap_barrier_crashes_recover_without_publishing_initialized"]
fn bootstrap_barrier_crash_child() {
    let manifest = PathBuf::from(std::env::var_os("REM_B6A_BOOTSTRAP_MANIFEST").unwrap());
    let parity = std::env::var("REM_B6A_PARITY").unwrap() == "true";
    let (f, state) = Fixture::new(parity);
    std::fs::write(&manifest, serde_json::to_vec(&f.config).unwrap()).unwrap();
    let mut actor = MountedDrive::with_transport(
        state,
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(BootstrapCrashTransport {
            inner: ModelTransport::new(f.world.clone(), DeviceRole::Drive { bay: BAY }),
            world: f.world.clone(),
            manifest,
            after_barrier: std::env::var("REM_B6A_AFTER_BARRIER").unwrap() == "true",
        }),
        f.root.path(),
        Some(UUID),
    )
    .unwrap();
    actor.open_session("default").unwrap();
    panic!("bootstrap crash cut was not reached");
}

#[test]
fn bootstrap_barrier_crashes_recover_without_publishing_initialized() {
    for parity in [false, true] {
        for after_barrier in [false, true] {
            let (f, state) = Fixture::new(parity);
            drop(state);
            let manifest = f.root.path().join("bootstrap-crash.json");
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "engine::drive_adapter::tests::pool_sessions::bootstrap_barrier_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("REM_B6A_BOOTSTRAP_MANIFEST", &manifest)
                .env("REM_B6A_PARITY", parity.to_string())
                .env("REM_B6A_AFTER_BARRIER", after_barrier.to_string())
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let config: remanence_state::RemConfig =
                serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
            let bot = std::fs::read(manifest.with_extension("bot")).unwrap();
            assert!(!remanence_state::TapeAuthority::exists(
                config.journal.dir.join("tapes"),
                UUID
            )
            .unwrap());
            f.world
                .lock()
                .unwrap()
                .tapes
                .get_mut("HAND")
                .unwrap()
                .records = vec![Record::Block(bot), Record::Filemark];
            let mut state = StateHandle::open_with_config(
                remanence_state::StatePaths::from_config(
                    config.daemon.state_dir.join("config.toml"),
                    &config,
                ),
                config.clone(),
            )
            .unwrap();
            // Existing crash recovery quarantines the orphaned mounted write;
            // neither a completed BOT nor a completed barrier invents authority.
            let mount = OperatorMount::with_transport(
                Path::new(DEVICE),
                &StaticAllowlist::new([SERIAL]),
                Box::new(Transport {
                    inner: ModelTransport::new(f.world.clone(), DeviceRole::Drive { bay: BAY }),
                    commands: f.commands.clone(),
                    fault: f.fault.clone(),
                    evidence: None,
                    last_opcode: None,
                }),
                &mut state,
                f.root.path(),
            )
            .unwrap();
            mount.finish().unwrap();
            let fences = state.catalog_index().list_active_tape_io_fences().unwrap();
            assert_eq!(fences.len(), 1);
            assert_eq!(fences[0].reason, "standalone_write_lost_by_restart");
            assert!(state
                .catalog_index()
                .non_terminal_sessions()
                .unwrap()
                .is_empty());
            assert!(!remanence_state::TapeAuthority::exists(
                config.journal.dir.join("tapes"),
                UUID
            )
            .unwrap());
            no_modifications(&f, 0);
            drop(state);
            std::fs::remove_dir_all(&config.daemon.state_dir).unwrap();
        }
    }
}
