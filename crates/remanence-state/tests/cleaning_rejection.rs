//! Ruling 16 regression: typed rejection precedes failed run closure and survives SQL loss.
use remanence_state::library_log::store::{Clock, Origin, StoreError, UuidSource};
use remanence_state::{library_log::*, projector, StateLockGuard};
use std::sync::Arc;

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
        Ok(TapeLookup::Contradiction)
    }
}

/// Rejection remains authoritative across the Reject/Finish crash cut and replay.
#[test]
fn rejected_cleaning_cartridge_stays_ineligible_across_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(StateLockGuard::acquire(root.path()).unwrap());
    let cartridge = cleaning_cartridge_ref(&"CLNREJECT".into());
    let mut authority = LibraryAuthority::open(
        &root.path().join("library"),
        [0xb3; 16],
        Origin {
            host: "test".into(),
            pid: 1,
            build: "b4b3-rejected-gap".into(),
        },
        Box::new(Fixed),
        Box::new(Fixed),
        false,
        &View,
        lease.clone(),
    )
    .unwrap();
    let mut sql = projector::open(root.path().join("typed.sqlite"))
        .unwrap()
        .write_handle();
    let apply = |sql: &mut remanence_state::CatalogIndex, r: AuthorityReceipt| {
        let reference = DecisionRef {
            sequence: r.receipt.sequence,
            record_uuid: r.receipt.record_uuid,
        };
        for record in r.applied {
            sql.apply(
                &projector::apply::AuthorityRecord::Library(Box::new(record)),
                projector::apply::ApplyPath::Live,
            )
            .unwrap();
        }
        reference
    };
    apply(
        &mut sql,
        authority
            .cleaning_recorded(
                CleaningRecorded {
                    run_id: "register".into(),
                    previous: None,
                    drive_uuid: None,
                    cartridge: Some(cartridge),
                    action: CleaningAction::Register(Register {
                        barcode: "CLNREJECT".into(),
                        home_slot: 0x400,
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
        authority
            .drive_observed(
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
        run_id: "run".into(),
        previous: None,
        drive_uuid: Some([9; 16]),
        cartridge: Some(cartridge),
        action: CleaningAction::Begin(Begin {
            trigger: CleanTrigger::Manual,
        }),
    };
    let mut begun = apply(
        &mut sql,
        authority
            .cleaning_recorded(run.clone(), AuditActor::System, SourceLayer::Layer4, &View)
            .unwrap(),
    );
    // Later run records may omit the selected cartridge; its binding must survive.
    let mut advance = run.clone();
    advance.previous = Some(begun);
    advance.cartridge = None;
    advance.action = CleaningAction::Advance(Advance {
        phase: CleanPhase::Verifying,
        home_slot: None,
    });
    begun = apply(
        &mut sql,
        authority
            .cleaning_recorded(advance, AuditActor::System, SourceLayer::Layer4, &View)
            .unwrap(),
    );
    let rejection = CleaningRecorded {
        run_id: "reject".into(),
        previous: None,
        drive_uuid: None,
        cartridge: Some(cartridge),
        action: CleaningAction::Reject(Reason {
            reason: "TapeAlert 20/21 remains set".into(),
        }),
    };
    apply(
        &mut sql,
        authority
            .cleaning_recorded(
                rejection.clone(),
                AuditActor::System,
                SourceLayer::Layer4,
                &View,
            )
            .unwrap(),
    );
    assert_eq!(
        sql.get_tape_cleaning_state(&cartridge.uuid)
            .unwrap()
            .flatten()
            .as_deref(),
        Some("rejected")
    );
    // Crash cut: durable Reject, no Finish. Reopen and reconstruct from the prefix.
    drop(authority);
    let mut cut = projector::open(root.path().join("cut.sqlite"))
        .unwrap()
        .write_handle();
    projector::thread::rebuild_library(
        &mut cut,
        &root.path().join("library"),
        [0xb3; 16],
        &View,
        projector::apply::ApplyPath::Rebuild,
    )
    .unwrap();
    assert_eq!(
        cut.get_tape_cleaning_state(&cartridge.uuid)
            .unwrap()
            .flatten()
            .as_deref(),
        Some("rejected")
    );
    assert_eq!(
        cut.get_active_clean_run_by_drive(&[9; 16])
            .unwrap()
            .unwrap()
            .run_id,
        "run"
    );
    authority = LibraryAuthority::open(
        &root.path().join("library"),
        [0xb3; 16],
        Origin {
            host: "test".into(),
            pid: 1,
            build: "b4b3-rejection".into(),
        },
        Box::new(Fixed),
        Box::new(Fixed),
        false,
        &View,
        lease,
    )
    .unwrap();
    let mut selection = run.clone();
    selection.run_id = "another-run".into();
    let mut credit = run.clone();
    credit.previous = Some(begun);
    credit.cartridge = None;
    credit.action = CleaningAction::Finish(Finish {
        result: CleanResult::Done,
        verified_credit: true,
    });
    for candidate in [rejection, selection.clone(), credit] {
        assert!(matches!(
            authority.cleaning_recorded(candidate, AuditActor::System, SourceLayer::Layer4, &View),
            Err(AuthorityError::Store(StoreError::Fold(
                FoldError::RetiredIdentity
            )))
        ));
    }
    run.previous = Some(begun);
    run.cartridge = None;
    run.action = CleaningAction::Finish(Finish {
        result: CleanResult::Failed,
        verified_credit: false,
    });
    let failed = apply(
        &mut sql,
        authority
            .cleaning_recorded(run, AuditActor::System, SourceLayer::Layer4, &View)
            .unwrap(),
    );
    assert!(sql
        .get_active_clean_run_by_drive(&[9; 16])
        .unwrap()
        .is_none());
    assert!(matches!(
        authority.cleaning_recorded(selection, AuditActor::System, SourceLayer::Layer4, &View),
        Err(AuthorityError::Store(StoreError::Fold(
            FoldError::RetiredIdentity
        )))
    ));
    apply(
        &mut sql,
        authority
            .alarm_changed(
                AlarmChanged {
                    condition_key: "cart-not-cleaning-behavior:CLNREJECT".into(),
                    change: GenerationChange {
                        expected: 0,
                        next: 1,
                    },
                    action: AlarmAction::Raise(AlarmRaise {
                        kind: AlarmKind::CartNotCleaningBehavior,
                        severity: Severity::Warning,
                        reason: "corroboration".into(),
                    }),
                    cause: Some(failed),
                },
                0,
                AuditActor::System,
                SourceLayer::Layer4,
                &View,
            )
            .unwrap(),
    );
    assert_eq!(
        sql.get_tape_cleaning_state(&cartridge.uuid)
            .unwrap()
            .flatten()
            .as_deref(),
        Some("rejected")
    );
    drop(authority);
    let mut rebuilt = projector::open(root.path().join("rebuilt.sqlite"))
        .unwrap()
        .write_handle();
    for path in [
        projector::apply::ApplyPath::Rebuild,
        projector::apply::ApplyPath::Startup,
    ] {
        projector::thread::rebuild_library(
            &mut rebuilt,
            &root.path().join("library"),
            [0xb3; 16],
            &View,
            path,
        )
        .unwrap();
        assert_eq!(
            rebuilt
                .get_tape_cleaning_state(&cartridge.uuid)
                .unwrap()
                .flatten()
                .as_deref(),
            Some("rejected")
        );
    }
}
