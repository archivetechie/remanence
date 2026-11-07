//! Ruling 17 regressions, flipped from the preserved continuation-3 reproductions.
//! Exercise the typed writer and SQL projection, including crash cuts and replay.
use remanence_state::library_log::store::{Clock, Origin, StoreError, UuidSource};
use remanence_state::{library_log::*, projector, CatalogIndex, StateLockGuard};
use std::sync::Arc;
struct Fixed;
impl Clock for Fixed {
    fn now(&self) -> u64 {
        1_793_664_000_000_000_000
    }
}
impl UuidSource for Fixed {
    fn next(&self, n: u64) -> Id {
        let mut id = [0x66; 16];
        id[8..].copy_from_slice(&n.to_be_bytes());
        id
    }
}
struct View;
impl TapeView for View {
    fn lookup(&self, _: &TapeAt) -> Result<TapeLookup, remanence_state::library_log::FoldError> {
        Ok(TapeLookup::Contradiction)
    }
}
fn authority(root: &std::path::Path) -> LibraryAuthority {
    LibraryAuthority::open(
        &root.join("library"),
        [0xb4; 16],
        Origin {
            host: "test".into(),
            pid: 1,
            build: "gap-audit".into(),
        },
        Box::new(Fixed),
        Box::new(Fixed),
        false,
        &View,
        Arc::new(StateLockGuard::acquire(root).unwrap()),
    )
    .unwrap()
}
fn apply(sql: &mut CatalogIndex, receipt: AuthorityReceipt) {
    for record in receipt.applied {
        sql.apply(
            &projector::apply::AuthorityRecord::Library(Box::new(record)),
            projector::apply::ApplyPath::Live,
        )
        .unwrap();
    }
}
/// Refresh updates evidence while preserving acknowledgement, including after SQL loss.
#[test]
fn acknowledged_alarm_refresh_preserves_acknowledgement() {
    let root = tempfile::tempdir().unwrap();
    let a = authority(root.path());
    let mut sql = projector::open(root.path().join("legacy.sqlite"))
        .unwrap()
        .write_handle();
    sql.raise_alarm(
        "condition",
        "cleaning-needs-operator",
        "warning",
        Some("first"),
    )
    .unwrap();
    sql.ack_alarm("condition", "user:operator").unwrap();
    let refreshed = sql
        .raise_alarm(
            "condition",
            "cleaning-needs-operator",
            "critical",
            Some("second"),
        )
        .unwrap();
    assert_eq!(refreshed.state, "acked");
    assert_eq!(refreshed.severity, "critical");
    assert_eq!(refreshed.detail.as_deref(), Some("second"));
    let mut typed = projector::open(root.path().join("typed.sqlite"))
        .unwrap()
        .write_handle();
    let mut record = AlarmChanged {
        condition_key: "condition".into(),
        change: GenerationChange {
            expected: 0,
            next: 1,
        },
        action: AlarmAction::Raise(AlarmRaise {
            kind: AlarmKind::CleaningNeedsOperator,
            severity: Severity::Warning,
            reason: "first".into(),
        }),
        cause: None,
    };
    apply(
        &mut typed,
        a.alarm_changed(
            record.clone(),
            0,
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap(),
    );
    record.action = AlarmAction::Acknowledge(Unit);
    apply(
        &mut typed,
        a.alarm_changed(
            record.clone(),
            1,
            AuditActor::User("operator".into()),
            SourceLayer::Layer4,
            &View,
        )
        .unwrap(),
    );
    record.action = AlarmAction::Refresh(AlarmRefresh {
        severity: Severity::Critical,
        reason: "second".into(),
        cause: None,
    });
    apply(
        &mut typed,
        a.alarm_changed(
            record.clone(),
            2,
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap(),
    );
    let check = |sql: &CatalogIndex| {
        let alarm = sql.get_alarm("condition").unwrap().unwrap();
        assert_eq!(alarm.state, "acked");
        assert_eq!(alarm.severity, "critical");
        assert_eq!(alarm.detail.as_deref(), Some("second"));
        assert_eq!(alarm.acked_by, refreshed.acked_by);
    };
    check(&typed);
    // Identical CAS retries are no-ops; older generations cannot overwrite evidence.
    assert!(a
        .alarm_changed(
            record.clone(),
            2,
            AuditActor::System,
            SourceLayer::Layer4,
            &View
        )
        .unwrap()
        .applied
        .is_empty());
    assert!(matches!(
        a.alarm_changed(
            record.clone(),
            1,
            AuditActor::System,
            SourceLayer::Layer4,
            &View
        ),
        Err(AuthorityError::Store(StoreError::Fold(
            FoldError::GenerationMismatch
        )))
    ));
    record.action = AlarmAction::Acknowledge(Unit);
    assert!(matches!(
        a.alarm_changed(
            record.clone(),
            3,
            AuditActor::User("again".into()),
            SourceLayer::Layer4,
            &View
        ),
        Err(AuthorityError::Store(StoreError::Fold(
            FoldError::AlarmState
        )))
    ));
    drop(a);
    let rebuilt = rebuild(root.path());
    check(&rebuilt);
}
/// Library-wide uncertainty and pre-provisioning family survive typed projection.
#[test]
fn readiness_scope_and_unprovisioned_family_survive() {
    let root = tempfile::tempdir().unwrap();
    let a = authority(root.path());
    let mut legacy = projector::open(root.path().join("legacy.sqlite"))
        .unwrap()
        .write_handle();
    let mut typed = projector::open(root.path().join("typed.sqlite"))
        .unwrap()
        .write_handle();
    let endpoint = remanence_state::DriveKey::new("LIB", 1);
    legacy
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id: uuid::Uuid::new_v4(),
            run_id: None,
            endpoint,
            changer_sg: None,
            drive_sg: None,
            drive_serial: None,
            barcode: Some("UNPROVISIONED".into()),
            source_slot: Some(23),
            media_generation: Some(9),
            media_family: None,
            phase: "startup_reconcile".into(),
            state: "aborted_unknown".into(),
            dirty_scope: Some("selected-library-snapshot".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
    let (id, receipt) = a
        .begin_readiness(
            ReadinessObserved {
                source_slot: None,
                run: None,
                cancel_source: None,
                scope: ReadinessScope::Library,
                media: Some(MediumFamily::Lto(LtoGen::Lto9)),
                operation: [0; 16],
                endpoint: DriveKey::Bay(Bay {
                    library_serial: "LIB".into(),
                    bay: 1,
                }),
                tape: None,
                barcode: Some("UNPROVISIONED".into()),
                phase: ReadinessPhase::StartupReconcile,
                state: ReadinessState::AbortedUnknown,
                previous: None,
                started_unix_ns: 1_793_664_000_000_000_000,
                limit_ms: Some(1000),
                evidence: None,
                load_intent: None,
                ready_generation: 1,
                result: ReadinessResult::AbortedUnknown,
            },
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    apply(&mut typed, receipt);
    let old = legacy
        .media_readiness_admission_conflicts("LIB", Some(2), None, false)
        .unwrap();
    let new = typed
        .media_readiness_admission_conflicts("LIB", Some(2), None, false)
        .unwrap();
    let row = typed
        .media_readiness_operation(uuid::Uuid::from_bytes(id))
        .unwrap()
        .unwrap();
    eprintln!("other-bay admission: legacy conflicts={}, typed conflicts={}; typed family={:?}, scope={:?}",old.len(),new.len(),row.media_generation,row.dirty_scope);
    assert_eq!(old.len(), 1);
    assert_eq!(new.len(), 1);
    assert_eq!(row.media_generation, Some(9));
    assert_eq!(
        row.dirty_scope.as_deref(),
        Some("selected-library-snapshot")
    );
    drop(a);
    let rebuilt = rebuild(root.path());
    assert_eq!(
        rebuilt
            .media_readiness_admission_conflicts("LIB", Some(2), None, false)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        rebuilt
            .media_readiness_operation(uuid::Uuid::from_bytes(id))
            .unwrap()
            .unwrap()
            .media_generation,
        Some(9)
    );
}
/// Retirement releases the label before inventory registration; replay preserves the cut.
#[test]
fn retired_data_cartridge_can_be_registered_after_crash() {
    let root = tempfile::tempdir().unwrap();
    let a = authority(root.path());
    let mut legacy = projector::open(root.path().join("legacy.sqlite"))
        .unwrap()
        .write_handle();
    let tape = cleaning_cartridge_ref(&"CLNKNOWN".into());
    legacy
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid: tape.uuid,
            voltag: "CLNKNOWN".into(),
            block_size: 262144,
            parity: remanence_parity::ParityConfig::None,
            force: false,
        })
        .unwrap();
    assert_eq!(
        legacy.reconcile_cleaning_prefixes(&["CLN".into()]).unwrap(),
        1
    );
    assert_eq!(
        legacy.get_tape(&tape.uuid).unwrap().unwrap().kind,
        "cleaning"
    );
    let mut typed = projector::open(root.path().join("typed.sqlite"))
        .unwrap()
        .write_handle();
    apply(
        &mut typed,
        a.reprovision(
            Reprovision {
                pool_source: None,
                operation: None,
                replaced_bot: None,
                tape,
                predecessor: None,
                mode: ProvisionMode::Initialize,
                label: Some("CLNKNOWN".into()),
                label_provenance: Some(LabelProvenance::Changer),
                parity_scheme: None,
                medium: MediumSpec {
                    family: MediumFamily::Lto(LtoGen::Lto9),
                    usable_logical_bytes: 0,
                    record_size: 262144,
                    rerecord: false,
                    lifecycle_ops: false,
                    source: GenerationSource::Operator,
                },
                pool: None,
                endpoint: None,
                adoption: None,
            },
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap(),
    );
    let register = CleaningRecorded {
        run_id: "register".into(),
        previous: None,
        drive_uuid: None,
        cartridge: Some(tape),
        action: CleaningAction::Register(Register {
            barcode: "CLNKNOWN".into(),
            home_slot: 23,
        }),
    };
    assert!(matches!(
        a.cleaning_recorded(
            register.clone(),
            AuditActor::System,
            SourceLayer::Layer4,
            &View
        ),
        Err(AuthorityError::Store(StoreError::Fold(
            FoldError::ConflictingDuplicate
        )))
    ));
    apply(
        &mut typed,
        a.retire(
            Retire {
                tape,
                reason: RetireReason::OperatorRequested,
                observed_tape_sequence: None,
                assignment_generation: 1,
                detail: Some("reclassified as cleaning by prefix".into()),
            },
            1,
            &View,
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap(),
    );
    // Durable crash cut: old data is retired, its label released, no cleaning identity yet.
    drop(a);
    drop(typed);
    let cut = rebuild(root.path());
    assert_eq!(cut.get_tape(&tape.uuid).unwrap().unwrap().state, "retired");
    assert!(cut.get_tape_by_voltag("CLNKNOWN").unwrap().is_none());
    drop(cut);
    let a = authority(root.path());
    let mut typed = projector::open(root.path().join("typed.sqlite"))
        .unwrap()
        .write_handle();
    apply(
        &mut typed,
        a.cleaning_recorded(
            register.clone(),
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap(),
    );
    let row = typed.get_tape_by_voltag("CLNKNOWN").unwrap().unwrap();
    assert_eq!(row.kind, "cleaning");
    assert_eq!(row.block_size, None);
    // A reconciliation retry sees the registered row and emits no second Register.
    assert!(matches!(
        a.cleaning_recorded(register, AuditActor::System, SourceLayer::Layer4, &View),
        Err(AuthorityError::Store(StoreError::Fold(
            FoldError::ConflictingDuplicate
        )))
    ));
    drop(a);
    let rebuilt = rebuild(root.path());
    assert_eq!(
        rebuilt
            .get_tape_by_voltag("CLNKNOWN")
            .unwrap()
            .unwrap()
            .kind,
        "cleaning"
    );
}

impl FinalizationView for View {
    fn committed_instance_count(&self, _: TapeRef) -> u64 {
        0
    }
    fn finalization_blocks_change(&self, _: TapeRef) -> bool {
        false
    }
}
/// Replay into an empty projection and then apply the same history a second time.
fn rebuild(root: &std::path::Path) -> CatalogIndex {
    let path = root.join(format!("rebuild-{}.sqlite", uuid::Uuid::new_v4()));
    let mut sql = projector::open(path).unwrap().write_handle();
    for mode in [
        projector::apply::ApplyPath::Rebuild,
        projector::apply::ApplyPath::Startup,
    ] {
        projector::thread::rebuild_library(
            &mut sql,
            &root.join("library"),
            [0xb4; 16],
            &View,
            mode,
        )
        .unwrap();
    }
    sql
}

/// A moved cartridge returns to the run's selected slot, including delayed binding.
#[test]
fn cleaning_run_home_survives_delayed_binding_and_replay() {
    for delayed_drive in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let a = authority(root.path());
        let path = root.path().join("homes.sqlite");
        let mut sql = projector::open(&path).unwrap().write_handle();
        let cartridge = cleaning_cartridge_ref(&"CLNHOME".into());
        apply(
            &mut sql,
            a.cleaning_recorded(
                CleaningRecorded {
                    run_id: "register".into(),
                    previous: None,
                    drive_uuid: None,
                    cartridge: Some(cartridge),
                    action: CleaningAction::Register(Register {
                        barcode: "CLNHOME".into(),
                        home_slot: 23,
                    }),
                },
                AuditActor::System,
                SourceLayer::Layer4,
                &View,
            )
            .unwrap(),
        );
        apply(
            &mut sql,
            a.drive_observed(
                DriveObserved {
                    drive_uuid: [9; 16],
                    serial: Some("DRIVE".into()),
                    identity_source: IdentitySource::DvcidAndInquiry,
                    endpoint: None,
                    managed: Managed::Rem,
                    inventory_generation: 1,
                    vendor: None,
                    product: None,
                    firmware_rev: None,
                },
                AuditActor::System,
                SourceLayer::Layer4,
                &View,
            )
            .unwrap(),
        );
        let mut run = CleaningRecorded {
            run_id: "run-home".into(),
            previous: None,
            drive_uuid: (!delayed_drive).then_some([9; 16]),
            cartridge: None,
            action: CleaningAction::Begin(Begin {
                trigger: CleanTrigger::Manual,
            }),
        };
        let receipt = a
            .cleaning_recorded(run.clone(), AuditActor::System, SourceLayer::Layer4, &View)
            .unwrap();
        let reference = DecisionRef {
            sequence: receipt.receipt.sequence,
            record_uuid: receipt.receipt.record_uuid,
        };
        apply(&mut sql, receipt);
        run.previous = Some(reference);
        run.cartridge = Some(cartridge);
        run.action = CleaningAction::Advance(Advance {
            phase: CleanPhase::MovingIn,
            home_slot: Some(42),
        });
        let selection = a
            .cleaning_recorded(run.clone(), AuditActor::System, SourceLayer::Layer4, &View)
            .unwrap();
        let reference = DecisionRef {
            sequence: selection.receipt.sequence,
            record_uuid: selection.receipt.record_uuid,
        };
        apply(&mut sql, selection);
        let db = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(
            db.query_row("select home_slot from library_cleaning_homes", [], |r| r
                .get::<_, u16>(0))
                .unwrap(),
            42
        );
        run.previous = Some(reference);
        run.drive_uuid = Some([9; 16]);
        run.cartridge = None; // Inherit the selected cartridge while binding the drive.
        run.action = CleaningAction::Advance(Advance {
            phase: CleanPhase::MovingBack,
            home_slot: None,
        });
        apply(
            &mut sql,
            a.cleaning_recorded(run, AuditActor::System, SourceLayer::Layer4, &View)
                .unwrap(),
        );
        let check = |sql: &CatalogIndex| {
            let run = sql.get_clean_run("run-home").unwrap().unwrap();
            assert_eq!(run.cart_home_slot, Some(42));
            assert_eq!(
                run.cart_tape_uuid.as_deref(),
                Some(cartridge.uuid.as_slice())
            );
        };
        check(&sql);
        drop(a);
        check(&rebuild(root.path()));
    }
}

/// A known tape uses its recorded family, never a barcode hint or caller override.
#[test]
fn readiness_uses_provisioned_family_and_rejects_override() {
    let root = tempfile::tempdir().unwrap();
    let a = authority(root.path());
    let mut sql = projector::open(root.path().join("known.sqlite"))
        .unwrap()
        .write_handle();
    let tape = TapeRef {
        uuid: [0x91; 16],
        incarnation: 1,
    };
    apply(
        &mut sql,
        a.reprovision(
            Reprovision {
                pool_source: None,
                operation: None,
                replaced_bot: None,
                tape,
                predecessor: None,
                mode: ProvisionMode::Initialize,
                label: Some("MISLEADL8".into()),
                label_provenance: Some(LabelProvenance::Changer),
                parity_scheme: None,
                medium: MediumSpec {
                    family: MediumFamily::Lto(LtoGen::Lto9),
                    usable_logical_bytes: 0,
                    record_size: 262144,
                    rerecord: false,
                    lifecycle_ops: false,
                    source: GenerationSource::Operator,
                },
                pool: None,
                endpoint: None,
                adoption: None,
            },
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap(),
    );
    let mut observation = ReadinessObserved {
        source_slot: None,
        run: None,
        cancel_source: None,
        operation: [0; 16],
        endpoint: DriveKey::Bay(Bay {
            library_serial: "LIB".into(),
            bay: 1,
        }),
        tape: Some(tape),
        barcode: Some("MISLEADL8".into()),
        phase: ReadinessPhase::StartupReconcile,
        state: ReadinessState::AbortedUnknown,
        previous: None,
        started_unix_ns: 1_793_664_000_000_000_000,
        limit_ms: Some(1000),
        evidence: None,
        load_intent: None,
        ready_generation: 1,
        result: ReadinessResult::AbortedUnknown,
        scope: ReadinessScope::default(),
        media: Some(MediumFamily::Lto(LtoGen::Lto8)),
    };
    assert!(matches!(
        a.begin_readiness(
            observation.clone(),
            AuditActor::System,
            SourceLayer::Layer4,
            &View
        ),
        Err(AuthorityError::Store(StoreError::Fold(
            FoldError::InvalidSchema
        )))
    ));
    observation.media = None;
    let (id, receipt) = a
        .begin_readiness(observation, AuditActor::System, SourceLayer::Layer4, &View)
        .unwrap();
    apply(&mut sql, receipt);
    let check = |sql: &CatalogIndex| {
        let row = sql
            .media_readiness_operation(uuid::Uuid::from_bytes(id))
            .unwrap()
            .unwrap();
        assert_eq!(row.media_generation, Some(9));
        assert_eq!(row.dirty_scope.as_deref(), Some("drive+tape"));
        assert!(sql
            .media_readiness_admission_conflicts("LIB", Some(2), None, false)
            .unwrap()
            .is_empty());
        assert_eq!(
            sql.media_readiness_admission_conflicts("LIB", Some(1), None, false)
                .unwrap()
                .len(),
            1
        );
    };
    check(&sql);
    drop(a);
    check(&rebuild(root.path()));
}

/// A run may fail before selection; neither live projection nor replay invents a home.
#[test]
fn cleaning_failure_before_selection_has_no_cartridge_or_home() {
    let root = tempfile::tempdir().unwrap();
    let a = authority(root.path());
    let mut sql = projector::open(root.path().join("unselected.sqlite"))
        .unwrap()
        .write_handle();
    let mut run = CleaningRecorded {
        run_id: "unselected".into(),
        previous: None,
        drive_uuid: Some([9; 16]),
        cartridge: None,
        action: CleaningAction::Begin(Begin {
            trigger: CleanTrigger::Manual,
        }),
    };
    let begin = a
        .cleaning_recorded(run.clone(), AuditActor::System, SourceLayer::Layer4, &View)
        .unwrap();
    run.previous = Some(DecisionRef {
        sequence: begin.receipt.sequence,
        record_uuid: begin.receipt.record_uuid,
    });
    apply(&mut sql, begin);
    for (phase, home_slot) in [
        (CleanPhase::MovingIn, None),
        (CleanPhase::MovingIn, Some(42)),
        (CleanPhase::MovingBack, Some(42)),
    ] {
        run.action = CleaningAction::Advance(Advance { phase, home_slot });
        assert!(matches!(
            a.cleaning_recorded(run.clone(), AuditActor::System, SourceLayer::Layer4, &View),
            Err(AuthorityError::Store(StoreError::Fold(
                FoldError::InvalidSchema
            )))
        ));
    }
    run.action = CleaningAction::Finish(Finish {
        result: CleanResult::Failed,
        verified_credit: false,
    });
    apply(
        &mut sql,
        a.cleaning_recorded(run, AuditActor::System, SourceLayer::Layer4, &View)
            .unwrap(),
    );
    let check = |sql: &CatalogIndex| {
        let run = sql.get_clean_run("unselected").unwrap().unwrap();
        assert_eq!(run.phase, "failed");
        assert_eq!(run.cart_tape_uuid, None);
        assert_eq!(run.cart_home_slot, None);
    };
    check(&sql);
    drop(a);
    check(&rebuild(root.path()));
}
