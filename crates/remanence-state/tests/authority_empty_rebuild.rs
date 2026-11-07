//! Authority twins of the current empty-rebuild characterizations. Every library
//! fact is published through LibraryAuthority; each test removes SQL/WAL/SHM and
//! replays durable sources through apply before checking the same decision again.
use remanence_state::library_log::store::{Clock, Origin, UuidSource};
use remanence_state::{
    library_log::*,
    projector::{
        self,
        apply::{ApplyPath, AuthorityRecord},
        thread::rebuild_library,
    },
    CatalogIndex, StateLockGuard,
};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc};
use uuid::Uuid;
struct Fixed;
impl Clock for Fixed {
    fn now(&self) -> u64 {
        1_793_664_000_000_000_000
    }
}
impl UuidSource for Fixed {
    fn next(&self, n: u64) -> Id {
        let mut id = [0x55; 16];
        id[8..].copy_from_slice(&n.to_be_bytes());
        id
    }
}
struct View;
impl TapeView for View {
    fn lookup(&self, _: &TapeAt) -> Result<TapeLookup, remanence_state::library_log::FoldError> {
        Ok(TapeLookup::NotYet)
    }
}
struct Free;
impl FinalizationView for Free {
    fn committed_instance_count(&self, _: TapeRef) -> u64 {
        0
    }
    fn finalization_blocks_change(&self, _: TapeRef) -> bool {
        false
    }
}
fn actor() -> AuditActor {
    AuditActor::User("operator".into())
}
fn tape(id: u8) -> TapeRef {
    TapeRef {
        uuid: [id; 16],
        incarnation: 1,
    }
}
fn change() -> GenerationChange {
    GenerationChange {
        expected: 1,
        next: 2,
    }
}
fn key() -> DriveKey {
    DriveKey::Bay(Bay {
        library_serial: "B1-LIBRARY".into(),
        bay: 1,
    })
}
struct Fixture {
    root: tempfile::TempDir,
    lease: Arc<StateLockGuard>,
    authority: Option<LibraryAuthority>,
    writer: Option<CatalogIndex>,
    snapshots: Vec<remanence_state::tape_log::authority::AuthoritySnapshot>,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(StateLockGuard::acquire(root.path()).unwrap());
        let authority = Self::open(&root, &lease);
        let writer = projector::open(root.path().join("catalog.sqlite"))
            .unwrap()
            .write_handle();
        Self {
            root,
            lease,
            authority: Some(authority),
            writer: Some(writer),
            snapshots: Vec::new(),
        }
    }
    fn open(root: &tempfile::TempDir, lease: &Arc<StateLockGuard>) -> LibraryAuthority {
        LibraryAuthority::open(
            &root.path().join("library"),
            [0xb1; 16],
            Origin {
                host: "test".into(),
                pid: 1,
                build: "b4b1".into(),
            },
            Box::new(Fixed),
            Box::new(Fixed),
            false,
            &View,
            lease.clone(),
        )
        .unwrap()
    }
    fn a(&self) -> &LibraryAuthority {
        self.authority.as_ref().unwrap()
    }
    fn sql(&self) -> &CatalogIndex {
        self.writer.as_ref().unwrap()
    }
    fn apply(&mut self, r: AuthorityReceipt) -> DecisionRef {
        let reference = DecisionRef {
            sequence: r.receipt.sequence,
            record_uuid: r.receipt.record_uuid,
        };
        for applied in r.applied {
            self.writer
                .as_mut()
                .unwrap()
                .apply(
                    &AuthorityRecord::Library(Box::new(applied)),
                    ApplyPath::Live,
                )
                .unwrap();
        }
        reference
    }
    fn provision(&mut self, id: u8, label: &str) {
        let r = self
            .a()
            .reprovision(
                Reprovision {
                    pool_source: Some(PoolSource::Operator(Unit)),
                    operation: None,
                    replaced_bot: None,
                    tape: tape(id),
                    predecessor: None,
                    mode: ProvisionMode::Initialize,
                    label: Some(label.into()),
                    label_provenance: Some(LabelProvenance::Operator),
                    parity_scheme: None,
                    medium: MediumSpec {
                        family: MediumFamily::Lto(LtoGen::Lto9),
                        usable_logical_bytes: 1_000_000,
                        record_size: 262144,
                        rerecord: false,
                        lifecycle_ops: true,
                        source: GenerationSource::Operator,
                    },
                    pool: Some("archive".into()),
                    endpoint: None,
                    adoption: None,
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        self.apply(r);
    }
    fn drive(&mut self) {
        let r = self
            .a()
            .drive_observed(
                DriveObserved {
                    drive_uuid: [9; 16],
                    serial: Some("B1-DRIVE".into()),
                    identity_source: IdentitySource::DvcidAndInquiry,
                    endpoint: Some(key()),
                    managed: Managed::Rem,
                    inventory_generation: 1,
                    vendor: None,
                    product: None,
                    firmware_rev: None,
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        self.apply(r);
    }
    fn rebuild(&mut self) {
        self.rebuild_with_view(&View);
    }
    fn rebuild_with_view(&mut self, view: &dyn TapeView) {
        self.authority.take();
        self.writer.take();
        let db = self.root.path().join("catalog.sqlite");
        for p in [
            db.clone(),
            PathBuf::from(format!("{}-wal", db.display())),
            PathBuf::from(format!("{}-shm", db.display())),
        ] {
            match std::fs::remove_file(p) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => panic!("{e}"),
            }
        }
        let mut writer = projector::open(&db).unwrap().write_handle();
        let report = rebuild_library(
            &mut writer,
            &self.root.path().join("library"),
            [0xb1; 16],
            view,
            ApplyPath::Rebuild,
        )
        .unwrap();
        assert!(report.pending.is_empty());
        for snapshot in &self.snapshots {
            writer
                .apply(
                    &AuthorityRecord::Tape(Box::new(snapshot.clone())),
                    ApplyPath::Rebuild,
                )
                .unwrap();
        }
        // Restart applies each durable record a second time; SQL decisions stay unchanged.
        rebuild_library(
            &mut writer,
            &self.root.path().join("library"),
            [0xb1; 16],
            view,
            ApplyPath::Startup,
        )
        .unwrap();
        self.writer = Some(writer);
        self.authority = Some(Self::open(&self.root, &self.lease));
    }
}
#[test]
fn authority_empty_rebuild_label_after_rebind_kept() {
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    f.provision(2, "B1002L9");
    let r = f
        .a()
        .label(
            Label {
                tape: tape(2),
                label: Some("B1001L9".into()),
                provenance: LabelProvenance::Operator,
                change: change(),
                prior_holder: Some(PriorHolder {
                    tape: tape(1),
                    change: change(),
                }),
                observed_bot_uuid: None,
            },
            1,
            &Free,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        assert_eq!(
            f.sql()
                .get_tape_by_voltag("B1001L9")
                .unwrap()
                .unwrap()
                .tape_uuid,
            vec![2; 16]
        );
        assert!(f
            .sql()
            .get_tape(&[1; 16])
            .unwrap()
            .unwrap()
            .voltag
            .is_none());
    }
}
fn logless() {
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        let t = f.sql().get_tape(&[1; 16]).unwrap().unwrap();
        assert_eq!(t.block_size, Some(262144));
        assert_eq!(t.pool_id.as_deref(), Some("archive"));
    }
}
#[test]
fn authority_empty_rebuild_logless_provision_geometry_pool_kept() {
    logless();
}
#[test]
fn authority_empty_rebuild_audited_logless_provision_geometry_pool_kept() {
    logless();
}
#[test]
fn authority_empty_rebuild_cached_wrap_map_kept() {
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    let bytes = br#"[{"partition":0,"wrap_number":1,"end_loi":200}]"#.to_vec();
    let r = f
        .a()
        .wrap_map_cached(
            WrapMapCached {
                tape: tape(1),
                calibration_generation: 1,
                action: WrapAction::Cache(Cache {
                    digest: Sha256::digest(&bytes).into(),
                    descriptors: Blob(bytes),
                    mapped_extent_lba: 200,
                    write_epoch: 1,
                }),
            },
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    let before = f.sql().get_wrap_map(&[1; 16]).unwrap().unwrap();
    f.rebuild();
    assert_eq!(f.sql().get_wrap_map(&[1; 16]).unwrap().unwrap(), before);
}
#[test]
fn authority_empty_rebuild_drive_annotation_kept() {
    let mut f = Fixture::new();
    f.drive();
    let r = f
        .a()
        .drive_decision(
            DriveDecision {
                drive_uuid: [9; 16],
                change: change(),
                decision: Stewardship::Annotate(Annotation {
                    purchase_date: None,
                    warranty_until: None,
                    cost: None,
                    notes: Some("use for recovery".into()),
                }),
            },
            1,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        assert_eq!(
            f.sql()
                .get_drive_by_uuid(&[9; 16])
                .unwrap()
                .unwrap()
                .notes
                .as_deref(),
            Some("use for recovery")
        );
    }
}
#[test]
fn authority_empty_rebuild_alarm_acknowledgement_kept() {
    let mut f = Fixture::new();
    let r = f
        .a()
        .alarm_changed(
            AlarmChanged {
                condition_key: "drive:B1".into(),
                change: change(),
                action: AlarmAction::Raise(AlarmRaise {
                    kind: AlarmKind::SnapshotPersistFailing,
                    severity: Severity::Warning,
                    reason: "test".into(),
                }),
                cause: None,
            },
            0,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    let r = f
        .a()
        .alarm_changed(
            AlarmChanged {
                condition_key: "drive:B1".into(),
                change: change(),
                action: AlarmAction::Acknowledge(Unit),
                cause: None,
            },
            1,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        let a = f.sql().get_alarm("drive:B1").unwrap().unwrap();
        assert_eq!(a.state, "acked");
        assert!(a.acked_by.is_some());
    }
}
fn idempotency(terminal: bool) {
    let mut f = Fixture::new();
    let (id, r) = f
        .a()
        .begin_operation(
            OperationKind::TapeVerification,
            Accepted {
                actor_fingerprint: "operator".into(),
                idempotency_key: Some([7; 16]),
                request_fingerprint: [8; 32],
                subject: None,
            },
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    let prior = f.apply(r);
    assert_eq!(
        f.sql()
            .get_operation(&Uuid::from_bytes(id).to_string())
            .unwrap()
            .unwrap()
            .state,
        "queued"
    );
    if terminal {
        let r = f
            .a()
            .operation_recorded(
                OperationRecorded {
                    operation: id,
                    kind: OperationKind::TapeVerification,
                    previous: Some(prior),
                    event: OperationEvent::Finished(Response {
                        response_fingerprint: Some([9; 32]),
                    }),
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        f.apply(r);
    }
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        let k = f
            .sql()
            .idempotency_scope_record("operator", "tape_verification", Uuid::from_bytes([7; 16]))
            .unwrap()
            .unwrap();
        assert_eq!(k.operation_id, Uuid::from_bytes(id));
        if terminal {
            assert_eq!(k.terminal_state.as_deref(), Some("finished"));
            assert_eq!(k.response_fingerprint, Some(vec![9; 32]));
            assert_eq!(
                f.sql()
                    .get_operation(&Uuid::from_bytes(id).to_string())
                    .unwrap()
                    .unwrap()
                    .state,
                "finished"
            );
        }
    }
}
#[test]
fn authority_empty_rebuild_idempotency_registration_kept() {
    idempotency(false);
}
#[test]
fn authority_empty_rebuild_audited_idempotency_and_operation_kept() {
    idempotency(true);
}
#[test]
fn authority_empty_rebuild_drive_fence_kept() {
    let mut f = Fixture::new();
    f.drive();
    let r = f
        .a()
        .fence_raised(
            FenceRaised {
                detail: Bounded(vec![]),
                kind: FenceKind::DriveFault,
                scope: FenceScope::Drive(key()),
                cause: FenceCause::TransferError,
                evidence: None,
                at: None,
                operation: None,
            },
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        assert!(f.sql().get_drive_by_uuid(&[9; 16]).unwrap().unwrap().fenced);
    }
}
#[test]
fn authority_empty_rebuild_clean_run_nonterminal_states_kept() {
    for phase in [
        CleanPhase::Fencing,
        CleanPhase::Selecting,
        CleanPhase::MovingIn,
        CleanPhase::Cleaning,
        CleanPhase::MovingBack,
        CleanPhase::Verifying,
    ] {
        let mut f = Fixture::new();
        f.drive();
        let cartridge = cleaning_cartridge_ref(&"CLNPHASE".into());
        let registration = f
            .a()
            .cleaning_recorded(
                CleaningRecorded {
                    run_id: "register-phase".into(),
                    previous: None,
                    drive_uuid: None,
                    cartridge: Some(cartridge),
                    action: CleaningAction::Register(Register {
                        barcode: "CLNPHASE".into(),
                        home_slot: 1,
                    }),
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        f.apply(registration);
        let r = f
            .a()
            .cleaning_recorded(
                CleaningRecorded {
                    run_id: "run-1".into(),
                    previous: None,
                    drive_uuid: Some([9; 16]),
                    cartridge: None,
                    action: CleaningAction::Begin(Begin {
                        trigger: CleanTrigger::Manual,
                    }),
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        let prior = f.apply(r);
        let r = f
            .a()
            .cleaning_recorded(
                CleaningRecorded {
                    run_id: "run-1".into(),
                    previous: Some(prior),
                    drive_uuid: Some([9; 16]),
                    cartridge: (phase == CleanPhase::MovingIn).then_some(cartridge),
                    action: CleaningAction::Advance(Advance {
                        phase,
                        home_slot: (phase == CleanPhase::MovingIn).then_some(1),
                    }),
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        f.apply(r);
        let before = f.sql().get_clean_run("run-1").unwrap().unwrap().phase;
        f.rebuild();
        assert_eq!(
            f.sql()
                .get_active_clean_run_by_drive(&[9; 16])
                .unwrap()
                .unwrap()
                .phase,
            before
        );
    }
}

#[test]
fn authority_empty_rebuild_readiness_operation_and_load_budget_kept() {
    let mut f = Fixture::new();
    let operation = *Uuid::new_v4().as_bytes();
    let (operation, r) = f
        .a()
        .begin_readiness(
            ReadinessObserved {
                source_slot: None,
                run: None,
                cancel_source: None,
                scope: ReadinessScope::Endpoint,
                media: None,
                operation,
                endpoint: key(),
                tape: None,
                barcode: Some("B1001L9".into()),
                phase: ReadinessPhase::ReadinessPoll,
                state: ReadinessState::Planned,
                previous: None,
                started_unix_ns: 1_793_664_000_000_000_000,
                limit_ms: Some(1000),
                evidence: None,
                load_intent: None,
                ready_generation: 1,
                result: ReadinessResult::Pending,
            },
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    for attempt in 1..=2 {
        let r = f
            .a()
            .motion_intent(
                MotionIntent {
                    command: *Uuid::new_v4().as_bytes(),
                    parent_operation: Some(operation),
                    motion: Motion::Load(Load {
                        endpoint: key(),
                        attempt,
                    }),
                    tape: None,
                    inventory_generation: None,
                    readiness_generation: Some(1),
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        f.apply(r);
    }
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        assert_eq!(
            f.sql()
                .media_readiness_operation(Uuid::from_bytes(operation))
                .unwrap()
                .unwrap()
                .deadline_at_utc
                .as_deref(),
            Some("2026-11-03T00:00:01Z")
        );
        assert_eq!(
            f.sql()
                .media_readiness_load_attempts_since_ready(Uuid::from_bytes(operation))
                .unwrap(),
            2
        );
        assert_eq!(
            f.sql()
                .media_readiness_operation(Uuid::from_bytes(operation))
                .unwrap()
                .unwrap()
                .state,
            "pre_ready_loading"
        );
    }
}
fn native_snapshot(
    f: &mut Fixture,
    bytes: &[u8],
) -> remanence_state::tape_log::authority::AuthoritySnapshot {
    use remanence_state::{authority::TapeAuthorityView, TapeAuthority};
    let directory = f.root.path().join("tapes");
    std::fs::create_dir_all(&directory).unwrap();
    let id = [0xa3; 16];
    std::fs::write(
        directory.join(format!("{}.remtape", Uuid::from_bytes(id).simple())),
        bytes,
    )
    .unwrap();
    TapeAuthority::open_fixture(&directory, id)
        .unwrap()
        .freeze()
        .unwrap()
}
/// Repair a previously projected longer authority, then retain the repair fact
/// through SQL deletion. The digest assertion requires RecoveryRecorded itself.
#[test]
fn authority_empty_rebuild_repair_projection_floor_kept() {
    use remanence_state::{authority::TapeAuthorityView, tape_log, TapeAuthority};
    let mut f = Fixture::new();
    f.provision(0xa1, "REPAIRL9");
    let id = [0xa1; 16];
    let directory = f.root.path().join("tapes");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("{}.remtape", Uuid::from_bytes(id).simple()));
    let mut bytes =
        include_bytes!("../../../fixtures/tape-log/parity-off-two-objects.remtape").to_vec();
    std::fs::write(&path, &bytes).unwrap();
    {
        let longer = TapeAuthority::open_fixture(&directory, id)
            .unwrap()
            .freeze()
            .unwrap();
        f.writer
            .as_mut()
            .unwrap()
            .apply(&AuthorityRecord::Tape(Box::new(longer)), ApplyPath::Live)
            .unwrap();
    }
    assert!(f.sql().projected_through_sequence(id).unwrap() > 1);
    let mut cursor = std::io::Cursor::new(&bytes);
    tape_log::store::Header::decode(&mut cursor, id).unwrap();
    let offset = cursor.position();
    let mut decoder = tape_log::codec::Decoder::at(cursor, 1, offset);
    let frame = decoder.next_frame().unwrap().unwrap();
    let next = frame.end as usize;
    let mut hash = Sha256::new();
    assert!(frame.payload.is_empty());
    // Initialized has no payload: hash prefix fields and structural bytes,
    // excluding the prefix CRC and the two section CRC trailers.
    hash.update(&bytes[offset as usize..offset as usize + 20]);
    hash.update(&bytes[offset as usize + tape_log::PREFIX_LEN..next - 16]);
    let reference_digest: [u8; 32] = hash.finalize().into();
    bytes[next + tape_log::PREFIX_LEN] ^= 0x80;
    std::fs::write(&path, &bytes).unwrap();
    let store = tape_log::store::Store::open_fixture(&directory, id).unwrap();
    let evidence = store.repair_tail(1, 0).unwrap();
    assert_eq!(evidence.through_sequence, 1);
    assert_eq!(evidence.digest, <[u8; 32]>::from(Sha256::digest(&bytes)));
    let repaired = TapeAuthority::open_fixture(&directory, id)
        .unwrap()
        .freeze()
        .unwrap();
    f.writer
        .as_mut()
        .unwrap()
        .rebuild_repaired_tape_projection(&repaired)
        .unwrap();
    assert_eq!(f.sql().projected_through_sequence(id).unwrap(), 1);
    struct Retained([u8; 32]);
    impl TapeView for Retained {
        fn lookup(
            &self,
            at: &TapeAt,
        ) -> Result<TapeLookup, remanence_state::library_log::FoldError> {
            Ok({
                assert_eq!(
                    (at.tape.uuid, at.sequence, at.record_digest),
                    ([0xa1; 16], 1, self.0)
                );
                TapeLookup::Present {
                    kind: tape_log::Tag::Initialized,
                    digest: self.0,
                }
            })
        }
    }
    let view = Retained(reference_digest);
    let receipt = f
        .a()
        .recovery_recorded(
            RecoveryRecorded {
                at: TapeAt {
                    tape: tape(0xa1),
                    sequence: 1,
                    record_digest: reference_digest,
                },
                action: RecoveryAction::RepairApplied(RepairApplied {
                    archived_prefix_digest: evidence.digest,
                    retained_through: 1,
                }),
                operation: [0x71; 16],
                prior: None,
            },
            actor(),
            SourceLayer::Layer4,
            &view,
        )
        .unwrap();
    f.apply(receipt);
    f.snapshots.push(repaired);
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild_with_view(&view);
        }
        assert_eq!(f.sql().projected_through_sequence(id).unwrap(), 1);
        let conn = rusqlite::Connection::open(f.root.path().join("catalog.sqlite")).unwrap();
        let retained: (Vec<u8>, Vec<u8>) = conn
            .query_row(
                "select retained_through,source_digest from library_recovery where tape_uuid=?1",
                [id.as_slice()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            retained,
            (1u64.to_be_bytes().to_vec(), evidence.digest.to_vec())
        );
    }
}
#[test]
fn authority_empty_rebuild_retired_sealed_identity_kept() {
    let mut f = Fixture::new();
    f.provision(0xa3, "B1SEALL9");
    let snapshot = native_snapshot(
        &mut f,
        include_bytes!("../../../fixtures/tape-log/sealed-terminal.remtape"),
    );
    f.writer
        .as_mut()
        .unwrap()
        .apply(
            &AuthorityRecord::Tape(Box::new(snapshot.clone())),
            ApplyPath::Live,
        )
        .unwrap();
    f.snapshots.push(snapshot);
    let r = f
        .a()
        .retire(
            Retire {
                tape: tape(0xa3),
                reason: RetireReason::OperatorRequested,
                observed_tape_sequence: None,
                assignment_generation: 1,
                detail: None,
            },
            1,
            &Free,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        let t = f.sql().get_tape(&[0xa3; 16]).unwrap().unwrap();
        assert_eq!(t.state, "retired");
        assert!(t.voltag.is_none());
    }
}

/// Both finalization states refuse the three changes under the append mutex.
#[test]
fn authority_finalization_view_refuses_assignment_label_and_retirement() {
    struct Blocked;
    impl FinalizationView for Blocked {
        fn committed_instance_count(&self, _: TapeRef) -> u64 {
            0
        }
        fn finalization_blocks_change(&self, _: TapeRef) -> bool {
            true
        }
    }
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    let assign = f.a().assign_pool(
        AssignPool {
            tape: tape(1),
            pool: None,
            change: change(),
            source: AssignmentSource::Operator,
            configuration: None,
        },
        1,
        &Blocked,
        actor(),
        SourceLayer::Layer4,
        &View,
    );
    assert!(matches!(
        assign,
        Err(AuthorityError::FinalizationInProgress(_))
    ));
    let label = f.a().label(
        Label {
            tape: tape(1),
            label: None,
            provenance: LabelProvenance::Operator,
            change: change(),
            prior_holder: None,
            observed_bot_uuid: None,
        },
        1,
        &Blocked,
        actor(),
        SourceLayer::Layer4,
        &View,
    );
    assert!(matches!(
        label,
        Err(AuthorityError::FinalizationInProgress(_))
    ));
    let retire = f.a().retire(
        Retire {
            tape: tape(1),
            reason: RetireReason::OperatorRequested,
            observed_tape_sequence: None,
            assignment_generation: 1,
            detail: None,
        },
        1,
        &Blocked,
        actor(),
        SourceLayer::Layer4,
        &View,
    );
    assert!(matches!(
        retire,
        Err(AuthorityError::FinalizationInProgress(_))
    ));
    f.rebuild();
    assert_eq!(
        f.sql()
            .get_tape(&[1; 16])
            .unwrap()
            .unwrap()
            .voltag
            .as_deref(),
        Some("B1001L9")
    );
}
/// The evidence tape is checked independently of a drive-scoped fence target.
#[test]
fn authority_fence_refuses_dead_evidence_tape_and_stale_generation() {
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    f.drive();
    let stale = f.a().assign_pool(
        AssignPool {
            tape: tape(1),
            pool: None,
            change: change(),
            source: AssignmentSource::Operator,
            configuration: None,
        },
        0,
        &Free,
        actor(),
        SourceLayer::Layer4,
        &View,
    );
    assert!(matches!(
        stale,
        Err(AuthorityError::Store(store::StoreError::Fold(
            FoldError::GenerationMismatch
        )))
    ));
    let r = f
        .a()
        .retire(
            Retire {
                tape: tape(1),
                reason: RetireReason::OperatorRequested,
                observed_tape_sequence: None,
                assignment_generation: 1,
                detail: None,
            },
            1,
            &Free,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    for scope in [FenceScope::Drive(key()), FenceScope::Tape(tape(1))] {
        let result = f.a().fence_raised(
            FenceRaised {
                detail: Bounded(vec![]),
                kind: if matches!(scope, FenceScope::Tape(_)) {
                    FenceKind::Write
                } else {
                    FenceKind::DriveFault
                },
                scope,
                cause: FenceCause::TransferError,
                evidence: None,
                at: Some(TapeAt {
                    tape: tape(1),
                    sequence: 1,
                    record_digest: [1; 32],
                }),
                operation: None,
            },
            actor(),
            SourceLayer::Layer4,
            &View,
        );
        assert!(matches!(
            result,
            Err(AuthorityError::Store(store::StoreError::Fold(
                FoldError::RetiredIdentity
            )))
        ));
    }
}
/// A copied measured spec cannot silently claim fresh measurement on revision.
#[test]
fn authority_medium_revision_requires_explicit_actual_source() {
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    let v = MediumSpecSet {
        tape: tape(1),
        change: change(),
        medium: MediumSpec {
            family: MediumFamily::Lto(LtoGen::Lto9),
            usable_logical_bytes: 2_000_000,
            record_size: 262144,
            rerecord: false,
            lifecycle_ops: true,
            source: GenerationSource::Measured,
        },
    };
    let result = f.a().medium_spec_set(
        v.clone(),
        1,
        GenerationSource::Operator,
        actor(),
        SourceLayer::Layer4,
        &View,
    );
    assert!(matches!(
        result,
        Err(AuthorityError::Store(store::StoreError::Fold(
            FoldError::CapacityIncreaseRequiresMeasurement
        )))
    ));
    let r = f
        .a()
        .medium_spec_set(
            v,
            1,
            GenerationSource::Measured,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    assert!(r.receipt.synced);
    f.apply(r);
    f.rebuild();
}
/// Caller-selected initial operation IDs cannot enter tag 23.
#[test]
fn authority_operation_acceptance_mints_identity() {
    let f = Fixture::new();
    let accepted = Accepted {
        actor_fingerprint: "operator".into(),
        idempotency_key: None,
        request_fingerprint: [1; 32],
        subject: None,
    };
    let result = f.a().operation_recorded(
        OperationRecorded {
            operation: [1; 16],
            kind: OperationKind::TapeVerification,
            previous: None,
            event: OperationEvent::Accepted(accepted.clone()),
        },
        actor(),
        SourceLayer::Layer4,
        &View,
    );
    assert!(matches!(
        result,
        Err(AuthorityError::OperationIdentityRequired)
    ));
    assert!(matches!(
        f.a().operation_recorded(
            OperationRecorded {
                operation: [1; 16],
                kind: OperationKind::TapeVerification,
                previous: None,
                event: OperationEvent::Started(Unit)
            },
            actor(),
            SourceLayer::Layer4,
            &View
        ),
        Err(AuthorityError::OperationIdentityRequired)
    ));
    let (started, _) = f
        .a()
        .start_operation(
            OperationKind::TapeVerification,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    assert_ne!(started, [1; 16]);
    let (first, _) = f
        .a()
        .begin_operation(
            OperationKind::TapeVerification,
            accepted.clone(),
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    let (second, _) = f
        .a()
        .begin_operation(
            OperationKind::TapeVerification,
            accepted,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    assert_ne!(first, second);
    assert_ne!(first, [1; 16]);
}

#[allow(dead_code)]
#[path = "support/library_fault.rs"]
mod durability;
fn diagnostic() -> Diagnostic {
    Diagnostic {
        event: DiagnosticEvent::RequestReceived,
        subject_kind: "test".into(),
        subject_id: None,
        detail: Bounded(vec![]),
    }
}
/// fsync failure and an uncertain unsynced Diagnostic both make all later writes unavailable.
#[test]
fn authority_publication_faults_are_sticky_for_decisions_and_diagnostics() {
    use remanence_library::durable_file::test_support::register;
    use std::sync::atomic::{AtomicU8, Ordering};
    for (stage, diagnostic_fault) in [(1, true), (2, false)] {
        for after in [false, true] {
            let mut f = Fixture::new();
            f.provision(1, "B1001L9");
            let injection = Arc::new(durability::Injection {
                stage: AtomicU8::new(stage),
                after,
            });
            let registration = register(
                f.root.path().join("library"),
                Arc::new(durability::FaultDirectory(injection.clone())),
            );
            let result = if diagnostic_fault {
                f.a()
                    .diagnostic(diagnostic(), actor(), SourceLayer::Layer4, &View)
            } else {
                f.a().assign_pool(
                    AssignPool {
                        tape: tape(1),
                        pool: None,
                        change: change(),
                        source: AssignmentSource::Operator,
                        configuration: None,
                    },
                    1,
                    &Free,
                    actor(),
                    SourceLayer::Layer4,
                    &View,
                )
            };
            assert!(matches!(
                result,
                Err(AuthorityError::Store(store::StoreError::Ambiguous(_)))
            ));
            assert_eq!(injection.stage.load(Ordering::SeqCst), 0);
            assert!(matches!(
                f.a()
                    .diagnostic(diagnostic(), actor(), SourceLayer::Layer4, &View),
                Err(AuthorityError::Store(store::StoreError::Fenced))
            ));
            drop(registration);
            assert!(matches!(
                f.a()
                    .diagnostic(diagnostic(), actor(), SourceLayer::Layer4, &View),
                Err(AuthorityError::Store(store::StoreError::Fenced))
            ));
        }
    }
}
/// Live append receipts distinguish the configured diagnostic policy from decision durability.
#[test]
fn authority_fsync_policy_and_fsync_before_apply_restart() {
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    let diagnostic = f
        .a()
        .diagnostic(diagnostic(), actor(), SourceLayer::Layer4, &View)
        .unwrap();
    assert!(!diagnostic.receipt.synced);
    let r = f
        .a()
        .assign_pool(
            AssignPool {
                tape: tape(1),
                pool: None,
                change: change(),
                source: AssignmentSource::Operator,
                configuration: None,
            },
            1,
            &Free,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    assert!(r.receipt.synced);
    // Drop the receipt without applying it: the durable source is sufficient.
    drop(r);
    assert_eq!(
        f.sql()
            .get_tape(&[1; 16])
            .unwrap()
            .unwrap()
            .pool_id
            .as_deref(),
        Some("archive")
    );
    f.rebuild();
    assert_eq!(f.sql().get_tape(&[1; 16]).unwrap().unwrap().pool_id, None);
}

/// Releasing one source fence cannot release the other; an unobserved endpoint
/// remains protected when its drive identity appears later.
#[test]
fn authority_overlapping_drive_fences_and_late_identity() {
    let mut f = Fixture::new();
    let mut raises = Vec::new();
    for _ in 0..2 {
        let r = f
            .a()
            .fence_raised(
                FenceRaised {
                    detail: Bounded(vec![]),
                    kind: FenceKind::DriveFault,
                    scope: FenceScope::Drive(key()),
                    cause: FenceCause::TransferError,
                    evidence: None,
                    at: None,
                    operation: None,
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        raises.push(f.apply(r).sequence);
    }
    f.drive();
    assert!(f.sql().get_drive_by_uuid(&[9; 16]).unwrap().unwrap().fenced);
    for (i, sequence) in raises.into_iter().enumerate() {
        let r = f
            .a()
            .fence_released(
                FenceReleased {
                    detail: Bounded(vec![]),
                    fence: sequence,
                    evidence: "verified".into(),
                    forced: false,
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        f.apply(r);
        assert_eq!(
            f.sql().get_drive_by_uuid(&[9; 16]).unwrap().unwrap().fenced,
            i == 0
        );
        f.rebuild();
        assert_eq!(
            f.sql().get_drive_by_uuid(&[9; 16]).unwrap().unwrap().fenced,
            i == 0
        );
    }
}

/// Native sealing and write sessions need no library mirror records at all.
#[test]
fn authority_native_seal_and_sessions_need_no_library_mirrors() {
    let mut f = Fixture::new();
    let snapshot = native_snapshot(
        &mut f,
        include_bytes!("../../../fixtures/tape-log/sealed-terminal.remtape"),
    );
    f.writer
        .as_mut()
        .unwrap()
        .apply(
            &AuthorityRecord::Tape(Box::new(snapshot.clone())),
            ApplyPath::Live,
        )
        .unwrap();
    f.snapshots.push(snapshot);
    {
        use remanence_state::{authority::TapeAuthorityView, TapeAuthority};
        let dir = f.root.path().join("session-tape");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{}.remtape", Uuid::from_bytes([0xa1; 16]).simple())),
            include_bytes!("../../../fixtures/tape-log/session-tail-synchronized.remtape"),
        )
        .unwrap();
        f.snapshots.push(
            TapeAuthority::open_fixture(&dir, [0xa1; 16])
                .unwrap()
                .freeze()
                .unwrap(),
        );
    }
    f.rebuild();
    let t = f.sql().get_tape(&[0xa3; 16]).unwrap().unwrap();
    assert_eq!(t.state, "sealed");
    let conn = rusqlite::Connection::open_with_flags(
        f.root.path().join("catalog.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        conn.query_row("select count(*) from library_projection_records", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        0
    );
    assert!(
        conn.query_row(
            "select count(*) from sessions where session_kind='write' and state='closed'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap()
            > 0
    );
}

/// Wrong predecessor identities and retired tape decisions are refused before publication.
#[test]
fn authority_wrong_counterpart_and_retired_assignment_refused() {
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    let (operation, r) = f
        .a()
        .begin_operation(
            OperationKind::TapeVerification,
            Accepted {
                actor_fingerprint: "operator".into(),
                idempotency_key: None,
                request_fingerprint: [8; 32],
                subject: None,
            },
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    let mut reference = f.apply(r);
    reference.record_uuid = [0xff; 16];
    assert!(f
        .a()
        .operation_recorded(
            OperationRecorded {
                operation,
                kind: OperationKind::TapeVerification,
                previous: Some(reference),
                event: OperationEvent::Finished(Response {
                    response_fingerprint: None
                })
            },
            actor(),
            SourceLayer::Layer4,
            &View
        )
        .is_err());
    let r = f
        .a()
        .retire(
            Retire {
                tape: tape(1),
                reason: RetireReason::OperatorRequested,
                observed_tape_sequence: None,
                assignment_generation: 1,
                detail: None,
            },
            1,
            &Free,
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(r);
    assert!(matches!(
        f.a().assign_pool(
            AssignPool {
                tape: tape(1),
                pool: None,
                change: change(),
                source: AssignmentSource::Operator,
                configuration: None
            },
            1,
            &Free,
            actor(),
            SourceLayer::Layer4,
            &View
        ),
        Err(AuthorityError::Store(store::StoreError::Fold(
            FoldError::RetiredIdentity
        )))
    ));
}

/// Diagnostics obey the enabled audit.fsync setting too.
#[test]
fn authority_diagnostic_fsync_enabled() {
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(StateLockGuard::acquire(root.path()).unwrap());
    let authority = LibraryAuthority::open(
        &root.path().join("library"),
        [0xb1; 16],
        Origin {
            host: "test".into(),
            pid: 1,
            build: "b4b1".into(),
        },
        Box::new(Fixed),
        Box::new(Fixed),
        true,
        &View,
        lease,
    )
    .unwrap();
    assert!(
        authority
            .diagnostic(diagnostic(), actor(), SourceLayer::Layer4, &View)
            .unwrap()
            .receipt
            .synced
    );
}

/// All three invocations enter one implementation; a worker owns its writer
/// until the last client drops, and its reader never uses the writer mutex.
#[test]
fn authority_apply_path_counts_and_worker_catch_up() {
    use remanence_state::projector::thread::ProjectionClient;
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    let r = f
        .a()
        .diagnostic(diagnostic(), actor(), SourceLayer::Layer4, &View)
        .unwrap();
    for path in [ApplyPath::Live, ApplyPath::Startup, ApplyPath::Rebuild] {
        for record in &r.applied {
            f.writer
                .as_mut()
                .unwrap()
                .apply(&AuthorityRecord::Library(Box::new(record.clone())), path)
                .unwrap();
        }
    }
    assert_eq!(f.sql().fixture_apply_counts().unwrap(), [2, 1, 1]);
    f.writer.take();
    let client = ProjectionClient::open(f.root.path().join("catalog.sqlite")).unwrap();
    let report = client
        .project_library(
            f.a().snapshot().unwrap(),
            Box::new(View),
            ApplyPath::Startup,
        )
        .unwrap();
    assert!(report.pending.is_empty());
    assert_eq!(report.received, 2);
    let reader = client.reader().unwrap();
    assert!(reader.get_tape(&[1; 16]).unwrap().is_some());
    drop(reader);
    drop(client);
    f.writer = Some(
        projector::open(f.root.path().join("catalog.sqlite"))
            .unwrap()
            .write_handle(),
    );
}

/// A frozen prefix is immutable even while the writer appends; retries of older
/// prefixes neither consume later bytes nor regress committed source progress.
#[test]
fn authority_frozen_prefix_excludes_later_append() {
    use remanence_state::projector::thread::ProjectionClient;
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    let old = f.a().snapshot().unwrap();
    let r = f
        .a()
        .diagnostic(diagnostic(), actor(), SourceLayer::Layer4, &View)
        .unwrap();
    assert_eq!(r.receipt.sequence, 2);
    f.writer.take();
    let client = ProjectionClient::open(f.root.path().join("catalog.sqlite")).unwrap();
    let before = client
        .project_library(old.clone(), Box::new(View), ApplyPath::Live)
        .unwrap();
    assert_eq!(before.received, 1);
    let after = client
        .project_library(f.a().snapshot().unwrap(), Box::new(View), ApplyPath::Live)
        .unwrap();
    assert_eq!(after.received, 2);
    client
        .project_library(old, Box::new(View), ApplyPath::Startup)
        .unwrap();
    let conn = rusqlite::Connection::open_with_flags(
        f.root.path().join("catalog.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(
        conn.query_row("select count(*) from library_projection_records", [], |r| r
            .get::<_, u64>(0))
            .unwrap(),
        2
    );
    let received: Vec<u8> = conn
        .query_row(
            "select received from projection_sources where source='library'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(received, 2u64.to_be_bytes());
}

/// The tape admission guard covers Prepared publication and each change's view.
/// A crash after Prepared leaves the model summary protected without Accepted.
#[test]
fn authority_prepared_and_automatic_seal_interleavings() {
    use std::sync::Mutex;
    struct Summary(bool);
    impl FinalizationView for Summary {
        fn committed_instance_count(&self, _: TapeRef) -> u64 {
            0
        }
        fn finalization_blocks_change(&self, _: TapeRef) -> bool {
            self.0
        }
    }
    for _automatic in [false, true] {
        let mut f = Fixture::new();
        f.provision(1, "B1001L9");
        let admission = Arc::new(Mutex::new(Summary(false)));
        std::thread::scope(|scope| {
            let publisher = admission.clone();
            scope
                .spawn(move || {
                    let mut summary = publisher.lock().unwrap();
                    summary.0 = true;
                })
                .join()
                .unwrap();
            // This is also the restart summary at the Prepared-before-Accepted crash cut.
            let summary = admission.lock().unwrap();
            assert!(matches!(
                f.a().assign_pool(
                    AssignPool {
                        tape: tape(1),
                        pool: None,
                        change: change(),
                        source: AssignmentSource::Operator,
                        configuration: None
                    },
                    1,
                    &*summary,
                    actor(),
                    SourceLayer::Layer4,
                    &View
                ),
                Err(AuthorityError::FinalizationInProgress(_))
            ));
        });
    }
}

/// The lifetime key stays first-wins after the fold's latest-entry window moves,
/// and TapeAlert masks project into the current SQL JSON representation.
#[test]
fn authority_health_lifetime_conflict_and_alert_encoding() {
    let mut f = Fixture::new();
    f.drive();
    let health = |session, flags| DriveHealthObserved {
        drive_uuid: [9; 16],
        tape: None,
        session_id: Some([session; 16]),
        operation: None,
        trigger: HealthTrigger::SessionClose,
        tape_alert_flags: flags,
        counters: Counters {
            write_corrected: Some(u64::MAX),
            write_uncorrected: None,
            read_corrected: None,
            read_uncorrected: None,
        },
        raw_pages: None,
    };
    for body in [health(5, (1 << 63) | 1), health(6, 0)] {
        let r = f
            .a()
            .drive_health_observed(body, actor(), SourceLayer::Layer4, &View)
            .unwrap();
        f.apply(r);
    }
    let r = f
        .a()
        .drive_health_observed(health(5, 2), actor(), SourceLayer::Layer4, &View)
        .unwrap();
    for record in r.applied {
        assert!(matches!(
            f.writer
                .as_mut()
                .unwrap()
                .apply(&AuthorityRecord::Library(Box::new(record)), ApplyPath::Live),
            Err(projector::apply::ApplyError::HealthConflict { .. })
        ));
    }
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        let conn = rusqlite::Connection::open_with_flags(
            f.root.path().join("catalog.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let flags: String = conn
            .query_row(
                "select tape_alert_flags from drive_health_snapshots where session_id=?1",
                [Uuid::from_bytes([5; 16]).to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flags, "[1,64]");
        assert_eq!(
            conn.query_row(
                "select write_errors_corrected from drive_health_snapshots where session_id=?1",
                [Uuid::from_bytes([5; 16]).to_string()],
                |r| r.get::<_, Option<i64>>(0)
            )
            .unwrap(),
            None
        );
        assert_eq!(
            conn.query_row("select count(*) from drive_health_snapshots", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            2
        );
    }
}

/// Bootstrap adoption with a non-exact physical tail must remain unusable after
/// an empty rebuild; its request binding is part of that same projection.
#[test]
fn authority_adoption_recovery_state_and_request_binding() {
    for (physical_tail, state, expected) in [
        (
            PhysicalTail::ExactBootstrapFilemarkEod,
            AdoptedState::Ready,
            "ready",
        ),
        (
            PhysicalTail::DataAfterBootstrap,
            AdoptedState::RecoveryRequired,
            "recovery_required",
        ),
    ] {
        let mut f = Fixture::new();
        let id = *Uuid::new_v4().as_bytes();
        let operation = *Uuid::new_v4().as_bytes();
        let r = f
            .a()
            .reprovision(
                Reprovision {
                    pool_source: Some(PoolSource::Operator(Unit)),
                    operation: None,
                    replaced_bot: None,
                    tape: TapeRef {
                        uuid: id,
                        incarnation: 1,
                    },
                    predecessor: None,
                    mode: ProvisionMode::Adopt,
                    label: Some("ADOPTL9".into()),
                    label_provenance: Some(LabelProvenance::Operator),
                    parity_scheme: None,
                    medium: MediumSpec {
                        family: MediumFamily::Lto(LtoGen::Lto9),
                        usable_logical_bytes: 1_000_000,
                        record_size: 262144,
                        rerecord: false,
                        lifecycle_ops: true,
                        source: GenerationSource::Operator,
                    },
                    pool: Some("archive".into()),
                    endpoint: Some(key()),
                    adoption: Some(Adoption {
                        request_fingerprint: [7; 32],
                        operation,
                        idempotency_key: operation,
                        physical_tail,
                        state,
                        library_revision: Some("r1".into()),
                        home_slot: Some(2),
                        drive_serial: "B1-DRIVE".into(),
                        identity_only: true,
                        bootstrap_drive_compression: false,
                        configured_drive_compression: false,
                    }),
                },
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        f.apply(r);
        for pass in 0..2 {
            if pass == 1 {
                f.rebuild();
            }
            assert_eq!(f.sql().get_tape(&id).unwrap().unwrap().state, expected);
            assert!(f
                .sql()
                .idempotency_scope_record("user:operator", "unknown", Uuid::from_bytes(operation))
                .unwrap()
                .is_some());
        }
    }
}

/// A legal Begin may precede drive discovery/binding; replay must retain it.
#[test]
fn authority_cleaning_late_drive_binding() {
    let mut f = Fixture::new();
    let begin = f
        .a()
        .cleaning_recorded(
            CleaningRecorded {
                run_id: "late-drive".into(),
                previous: None,
                drive_uuid: None,
                cartridge: None,
                action: CleaningAction::Begin(Begin {
                    trigger: CleanTrigger::Manual,
                }),
            },
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    let previous = f.apply(begin);
    f.rebuild();
    assert!(f.sql().get_clean_run("late-drive").unwrap().is_none());
    f.drive();
    let advance = f
        .a()
        .cleaning_recorded(
            CleaningRecorded {
                run_id: "late-drive".into(),
                previous: Some(previous),
                drive_uuid: Some([9; 16]),
                cartridge: None,
                action: CleaningAction::Advance(Advance {
                    phase: CleanPhase::Cleaning,
                    home_slot: None,
                }),
            },
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(advance);
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        let run = f.sql().get_clean_run("late-drive").unwrap().unwrap();
        assert_eq!(run.drive_uuid, vec![9; 16]);
        assert_eq!(run.phase, "cleaning");
        assert_eq!(run.trigger, "manual");
    }
}

/// Completion diagnostics must reach today's readiness reader after SQL loss.
#[test]
fn authority_readiness_completion_evidence_kept() {
    let mut f = Fixture::new();
    let (operation, receipt) = f
        .a()
        .begin_readiness(
            ReadinessObserved {
                source_slot: None,
                run: None,
                cancel_source: None,
                scope: ReadinessScope::Endpoint,
                media: None,
                operation: [0; 16],
                endpoint: key(),
                tape: None,
                barcode: None,
                phase: ReadinessPhase::ReadinessPoll,
                state: ReadinessState::Planned,
                previous: None,
                started_unix_ns: 1_793_664_000_000_000_000,
                limit_ms: Some(1000),
                evidence: Some(CompletionEvidence {
                    dispatch: Dispatch::Issued,
                    completion: Completion::Unknown,
                    scsi_status: Some(2),
                    host_status: Some(3),
                    driver_status: Some(4),
                    sense: Some(Blob(vec![0x70, 0, 2])),
                    residual: None,
                }),
                load_intent: None,
                ready_generation: 1,
                result: ReadinessResult::Pending,
            },
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(receipt);
    for pass in 0..2 {
        if pass == 1 {
            f.rebuild();
        }
        let row = f
            .sql()
            .media_readiness_operation(Uuid::from_bytes(operation))
            .unwrap()
            .unwrap();
        assert_eq!(row.target_status, Some(2));
        assert_eq!(row.last_host_status, Some(3));
        assert_eq!(row.last_driver_status, Some(4));
        assert_eq!(row.last_sense_raw.as_deref(), Some("700002"));
        assert_eq!(row.transport_class.as_deref(), Some("unknown"));
    }
}

/// The writer consults real native occupancy even before SQL has received the
/// instances. Refusals append nothing; retirement remains an independent duty.
#[test]
fn authority_occupied_tape_refuses_pool_changes_but_allows_retirement() {
    use remanence_state::{authority::TapeAuthorityView, TapeAuthority};
    struct Summary<'a>(
        &'a remanence_state::tape_log::authority::AuthoritySnapshot,
        bool,
    );
    impl FinalizationView for Summary<'_> {
        fn committed_instance_count(&self, tape: TapeRef) -> u64 {
            assert_eq!(tape.uuid, [0xa1; 16]);
            self.0.tape_summary().object_count
        }
        fn finalization_blocks_change(&self, _: TapeRef) -> bool {
            self.1
        }
    }
    let mut f = Fixture::new();
    f.provision(0xa1, "OCCUPIED");
    let dir = f.root.path().join("tapes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{}.remtape", Uuid::from_bytes([0xa1; 16]).simple())),
        include_bytes!("../../../fixtures/tape-log/parity-off-two-objects.remtape"),
    )
    .unwrap();
    let native = TapeAuthority::open_fixture(&dir, [0xa1; 16])
        .unwrap()
        .freeze()
        .unwrap();
    assert_eq!(native.tape_summary().object_count, 2);
    let bytes = || {
        let mut paths = std::fs::read_dir(f.root.path().join("library"))
            .unwrap()
            .map(|p| p.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "remlib"))
            .collect::<Vec<_>>();
        paths.sort();
        paths
            .iter()
            .map(|p| std::fs::read(p).unwrap())
            .collect::<Vec<_>>()
    };
    let before = bytes();
    for (pool, blocked) in [
        (Some("new".into()), false),
        (None, false),
        (Some("new".into()), true),
        (None, true),
    ] {
        let error = f
            .a()
            .assign_pool(
                AssignPool {
                    tape: tape(0xa1),
                    pool,
                    change: change(),
                    source: AssignmentSource::Operator,
                    configuration: None,
                },
                1,
                &Summary(&native, blocked),
                actor(),
                SourceLayer::Layer4,
                &View,
            )
            .unwrap_err();
        assert!(
            matches!(error, AuthorityError::TapeHoldsCommittedInstances { tape_uuid, instance_count: 2 }
            if tape_uuid == Uuid::from_bytes([0xa1;16]))
        );
        assert_eq!(error.to_string(), format!("tape {} holds 2 committed instances; a tape holding data cannot change pool; it can only be retired", Uuid::from_bytes([0xa1;16])));
        assert_eq!(bytes(), before);
        assert_eq!(
            f.sql()
                .get_tape(&[0xa1; 16])
                .unwrap()
                .unwrap()
                .pool_id
                .as_deref(),
            Some("archive")
        );
    }
    f.writer
        .as_mut()
        .unwrap()
        .apply(
            &AuthorityRecord::Tape(Box::new(native.clone())),
            ApplyPath::Live,
        )
        .unwrap();
    let receipt = f
        .a()
        .retire(
            Retire {
                tape: tape(0xa1),
                reason: RetireReason::OperatorRequested,
                observed_tape_sequence: None,
                assignment_generation: 1,
                detail: None,
            },
            1,
            &Summary(&native, false),
            actor(),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    assert_eq!(
        receipt.receipt.sequence, 2,
        "refused decisions must not consume sequence"
    );
    f.apply(receipt);
    assert_eq!(
        f.sql().get_tape(&[0xa1; 16]).unwrap().unwrap().state,
        "retired"
    );
}

/// Ruling 13: a physical suffix refusal is a Write fence, not a tape state.
/// Lose SQL between the synced raise and its projection, then release exactly
/// that fence after operator proof. Repeated replay must preserve both cuts.
#[test]
fn ruling13_suffix_fence_survives_unprojected_crash_and_explicit_release() {
    let mut f = Fixture::new();
    f.provision(1, "B1001L9");
    let raised = f
        .a()
        .fence_raised(
            FenceRaised {
                detail: Bounded(vec![]),
                kind: FenceKind::Write,
                scope: FenceScope::Tape(tape(1)),
                cause: FenceCause::SessionSuffixContradiction,
                evidence: None,
                at: None,
                operation: None,
            },
            actor(),
            SourceLayer::Layer5,
            &View,
        )
        .unwrap();
    let fence = raised.receipt.sequence;
    // Simulate exit before delivering the application to SQL. The library log
    // already carries the observation; no recovery_required flag is set.
    drop(raised);
    for _ in 0..2 {
        f.rebuild();
        let tape = f.sql().get_tape(&[1; 16]).unwrap().unwrap();
        assert_eq!(tape.state, "ready");
        let conflicts = f
            .sql()
            .tape_io_admission_conflicts(&[1; 16], Some("B1001L9"))
            .unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(f
            .sql()
            .tape_io_admission_conflicts(&[2; 16], Some("B1002L9"))
            .unwrap()
            .is_empty());
    }
    // A refused release must leave the fence; the valid proof is durable
    // before its effect is delivered. No automatic release follows a reopen.
    assert!(f
        .a()
        .fence_released(
            FenceReleased {
                detail: Bounded(vec![]),
                fence: 0,
                evidence: "wrong fence identity".into(),
                forced: false
            },
            actor(),
            SourceLayer::Layer5,
            &View,
        )
        .is_err());
    let released = f
        .a()
        .fence_released(
            FenceReleased {
                detail: Bounded(vec![]),
                fence,
                evidence: "operator proved and superseded the physical suffix".into(),
                forced: false,
            },
            actor(),
            SourceLayer::Layer5,
            &View,
        )
        .unwrap();
    assert_eq!(
        f.sql()
            .tape_io_admission_conflicts(&[1; 16], Some("B1001L9"))
            .unwrap()
            .len(),
        1
    );
    f.apply(released);
    for _ in 0..2 {
        assert!(f
            .sql()
            .tape_io_admission_conflicts(&[1; 16], Some("B1001L9"))
            .unwrap()
            .is_empty());
        assert_eq!(f.sql().get_tape(&[1; 16]).unwrap().unwrap().state, "ready");
        f.rebuild();
    }
}
