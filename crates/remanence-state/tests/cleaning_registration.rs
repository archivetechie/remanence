//! Ruling 15 regression, flipped from the continuation-1 reproduction: cleaning
//! registration creates inventory identity and rebuilds without data geometry.
use remanence_state::library_log::store::{Clock, Origin, StoreError, UuidSource};
use remanence_state::{library_log::*, projector, StateLockGuard};
use std::sync::Arc;
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
        Ok(TapeLookup::Contradiction)
    }
}

/// Public registration matches the existing barcode identity, then survives SQL loss.
#[test]
fn inventory_only_cleaning_registration_survives_sql_loss() {
    let root = tempfile::tempdir().unwrap();
    let lease = Arc::new(StateLockGuard::acquire(root.path()).unwrap());
    let mut index = projector::open(root.path().join("catalog.sqlite"))
        .unwrap()
        .write_handle();
    let existing = index.ensure_cleaning_cartridge("CLNU01L9").unwrap();
    assert_eq!(existing.block_size, None);
    assert_eq!(existing.kind, "cleaning");
    let tape = TapeRef {
        uuid: *Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            b"remanence:cleaning-cartridge:CLNU01L9",
        )
        .as_bytes(),
        incarnation: 1,
    };
    assert_eq!(existing.tape_uuid, tape.uuid);
    let authority = LibraryAuthority::open(
        &root.path().join("library"),
        [0xb3; 16],
        Origin {
            host: "test".into(),
            pid: 1,
            build: "b4b3-reproduction".into(),
        },
        Box::new(Fixed),
        Box::new(Fixed),
        false,
        &View,
        lease,
    )
    .unwrap();
    let registration = CleaningRecorded {
        run_id: "register:CLNU01L9".into(),
        previous: None,
        drive_uuid: None,
        cartridge: Some(tape),
        action: CleaningAction::Register(Register {
            barcode: "CLNU01L9".into(),
            home_slot: 0x400,
        }),
    };
    let receipt = authority
        .cleaning_recorded(
            registration.clone(),
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    // Apply to a fresh projection, not the legacy row used to compare identity.
    let mut projected = projector::open(root.path().join("fresh.sqlite"))
        .unwrap()
        .write_handle();
    for applied in receipt.applied {
        projected
            .apply(
                &projector::apply::AuthorityRecord::Library(Box::new(applied)),
                projector::apply::ApplyPath::Live,
            )
            .unwrap();
    }
    let check = |index: &remanence_state::CatalogIndex, database: &str| {
        let row = index.get_tape(&tape.uuid).unwrap().unwrap();
        assert_eq!(row.kind, "cleaning");
        assert_eq!(row.block_size, None);
        assert_eq!(row.voltag.as_deref(), Some("CLNU01L9"));
        let sql = rusqlite::Connection::open_with_flags(
            root.path().join(database),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let home: u16 = sql
            .query_row(
                "select home_slot from library_cleaning_homes where tape_uuid=?1",
                [tape.uuid.as_slice()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(home, 0x400);
    };
    check(&projected, "fresh.sqlite");
    for (cartridge, expected) in [
        (Some(tape), FoldError::ConflictingDuplicate),
        (None, FoldError::Chain),
    ] {
        let mut candidate = registration.clone();
        candidate.cartridge = cartridge;
        let error = authority
            .cleaning_recorded(candidate, AuditActor::System, SourceLayer::Layer4, &View)
            .unwrap_err();
        assert!(
            matches!(error, AuthorityError::Store(StoreError::Fold(ref actual)) if *actual == expected)
        );
    }
    let error = authority
        .reprovision(
            Reprovision {
                pool_source: None,
                operation: None,
                replaced_bot: None,
                tape,
                predecessor: None,
                mode: ProvisionMode::Initialize,
                label: Some("CLNU01L9".into()),
                label_provenance: Some(LabelProvenance::Changer),
                parity_scheme: None,
                // Even a guessed LTO family cannot represent absent geometry.
                medium: MediumSpec {
                    family: MediumFamily::Lto(LtoGen::Lto9),
                    usable_logical_bytes: 0,
                    record_size: 0,
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
        .unwrap_err();
    eprintln!("Reprovision absent record geometry: {error:?}");
    assert!(matches!(
        error,
        AuthorityError::Store(StoreError::Fold(FoldError::WrongMediumKind))
    ));
    drop(authority);
    drop(projected);
    // Rebuild a second empty SQL database, then replay twice to check idempotence.
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
        check(&rebuilt, "rebuilt.sqlite");
    }
}
