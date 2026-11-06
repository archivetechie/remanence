//! Real-file native evidence tests; fault injection never belongs to the authority module.
use super::*;
use crate::library_log::TapeRef;
use std::io::Write;
const ID: Id = [0xa1; 16];
fn fixture(dir: &Path) -> std::path::PathBuf {
    let path = dir.join(format!("{}.remtape", uuid::Uuid::from_bytes(ID).simple()));
    fs::write(
        &path,
        include_bytes!("../../../../fixtures/tape-log/session-tail-synchronized.remtape"),
    )
    .unwrap();
    path
}
fn probe(dir: &Path) -> Result<NativeTapeView, StoreError> {
    NativeTapeView::probe_with(dir, |id| Store::open_fixture(dir, id))
}
fn at(sequence: u64) -> TapeAt {
    TapeAt {
        tape: TapeRef {
            uuid: ID,
            incarnation: 1,
        },
        sequence,
        record_digest: [0; 32],
    }
}

#[test]
fn native_view_probes_all_logs_and_never_returns_not_yet() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path());
    let invalid = dir.path().join(format!(
        "{}.remtape",
        uuid::Uuid::from_bytes([0xa2; 16]).simple()
    ));
    fs::write(&invalid, b"invalid header").unwrap();
    let view = probe(dir.path()).unwrap();
    assert!(matches!(
        view.lookup(&at(1)).unwrap(),
        TapeLookup::Present {
            kind: crate::tape_log::Tag::Initialized,
            ..
        }
    ));
    assert_eq!(
        view.lookup(&at(u64::MAX)).unwrap(),
        TapeLookup::Contradiction
    );
    assert!(matches!(
        view.lookup(&at(0)).unwrap(),
        TapeLookup::EmptyPrefix { .. }
    ));
    for id in [[0xa2; 16], [0xa3; 16]] {
        let mut missing = at(1);
        missing.tape.uuid = id;
        assert_eq!(view.lookup(&missing).unwrap(), TapeLookup::Contradiction);
    }
    let mut wrong = at(1);
    wrong.tape.incarnation = 2;
    assert_eq!(view.lookup(&wrong).unwrap(), TapeLookup::Contradiction);
    // An unrelated unreadable entry must fail preflight before any replay.
    fs::remove_file(&invalid).unwrap();
    fs::create_dir(&invalid).unwrap();
    assert!(matches!(probe(dir.path()), Err(StoreError::Io(_))));
    assert!(matches!(
        probe(&dir.path().join("absent")),
        Err(StoreError::Io(_))
    ));
}

#[test]
fn native_view_corruption_is_a_contradiction_but_later_read_errors_fail() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let view = probe(dir.path()).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(0)
        .unwrap();
    for sequence in [0, 1] {
        assert!(matches!(
            view.lookup(&at(sequence)),
            Err(FoldError::TapeUnavailable(_))
        ));
    }
    // On a new full probe this zero-length header is proved corrupt.
    assert_eq!(
        probe(dir.path()).unwrap().lookup(&at(1)).unwrap(),
        TapeLookup::Contradiction
    );
    fixture(dir.path());
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[0xff; 28])
        .unwrap();
    assert_eq!(
        probe(dir.path()).unwrap().lookup(&at(1)).unwrap(),
        TapeLookup::Contradiction
    );
}

#[test]
fn native_view_serves_repair_fenced_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = fixture(dir.path());
    let store = Store::open_fixture(dir.path(), ID).unwrap();
    let snapshot = store.snapshot().unwrap();
    let through = snapshot.summary().last_closed_sequence;
    let mut view = probe(dir.path()).unwrap();
    view.refresh(snapshot).unwrap();
    let expected = view.lookup(&at(through)).unwrap();
    assert!(matches!(expected, TapeLookup::Present { .. }));
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(&[0xff; 28])
        .unwrap();
    store.repair_tail(through, through).unwrap();
    assert!(path.with_extension("repair-required").exists());
    assert_eq!(
        probe(dir.path()).unwrap().lookup(&at(through)).unwrap(),
        expected
    );
    assert_eq!(view.lookup(&at(through)).unwrap(), expected);
}

/// A durable empty prefix is evidence, whereas an absent log is not. Supersede
/// retries bind to the same header and keep a zero repair floor after reopen.
#[test]
fn fresh_supersede_uses_existing_header_and_rejects_replaced_identity() {
    use crate::library_log::store::{Origin, StoreError as LibraryStoreError};
    use crate::library_log::{tests::*, *};
    use std::sync::{atomic::AtomicU64, Arc};
    let root = tempfile::tempdir().unwrap();
    let native_dir = root.path().join("native");
    fs::create_dir(&native_dir).unwrap();
    let native = Store::new_fixture(
        &native_dir,
        crate::tape_log::store::Header {
            tape_uuid: tape(1).uuid,
            block_size: 262144,
            scheme: None,
        },
    )
    .unwrap();
    let mut lease = native.acquire(true).unwrap();
    let mut at = at(1, 0);
    at.tape = tape(1);
    let absent = probe(&native_dir).unwrap();
    assert_eq!(absent.lookup(&at).unwrap(), TapeLookup::Contradiction);
    lease.publish_empty_prefix().unwrap();
    at.record_digest = lease.snapshot().unwrap().header_digest().unwrap().unwrap();
    drop(lease);
    let view = probe(&native_dir).unwrap();
    let state_lock = Arc::new(crate::StateLockGuard::acquire(root.path()).unwrap());
    let open = |view: &dyn TapeView| {
        LibraryAuthority::open(
            &root.path().join("library"),
            LOG,
            Origin {
                host: "test".into(),
                pid: 1,
                build: "ruling19".into(),
            },
            Box::new(FixedClock(Arc::new(AtomicU64::new(NOW)))),
            Box::new(Ids),
            false,
            view,
            state_lock.clone(),
        )
        .unwrap()
    };
    let authority = open(&view);
    let Body::Reprovision(provision) = provision(1, None) else {
        unreachable!()
    };
    authority
        .reprovision(provision, AuditActor::System, SourceLayer::Layer4, &view)
        .unwrap();
    let value = SupersedeTail {
        at: at.clone(),
        operation: None,
        session_id: None,
        preview_digest: None,
        tail_file_count: 0,
    };
    let receipt = authority
        .supersede_tail(
            value.clone(),
            AuditActor::System,
            SourceLayer::Layer4,
            &view,
        )
        .unwrap();
    assert_eq!(
        authority
            .supersede_tail(
                value.clone(),
                AuditActor::System,
                SourceLayer::Layer4,
                &view
            )
            .unwrap()
            .receipt,
        receipt.receipt
    );
    let mut conflict = value.clone();
    conflict.preview_digest = Some([9; 32]);
    assert!(matches!(
        authority.supersede_tail(conflict, AuditActor::System, SourceLayer::Layer4, &view),
        Err(AuthorityError::Store(LibraryStoreError::Fold(
            FoldError::ConflictingDuplicate
        )))
    ));
    let mut bad = value.clone();
    bad.at.record_digest[0] ^= 1;
    assert!(matches!(
        authority.supersede_tail(bad, AuditActor::System, SourceLayer::Layer4, &view),
        Err(AuthorityError::Store(LibraryStoreError::Fold(
            FoldError::TapeContradiction
        )))
    ));
    let Body::VerificationVerdict(mut verdict) = verdict() else {
        unreachable!()
    };
    verdict.at = at.clone();
    assert!(matches!(
        authority.verification_verdict(verdict, AuditActor::System, SourceLayer::Layer4, &view),
        Err(AuthorityError::Store(LibraryStoreError::Fold(
            FoldError::InvalidSchema
        )))
    ));
    // The durable library acknowledgement precedes the bootstrap/filemark
    // barrier record. Publication uses the production native authority owner.
    let native_authority = crate::TapeAuthority::open_fixture(&native_dir, tape(1).uuid).unwrap();
    native_authority
        .acquire_exclusive()
        .unwrap()
        .publish_initialized(
            262144,
            None,
            remanence_parity::CommitCursor {
                partition: 0,
                lba: 2,
            },
        )
        .unwrap();
    let view = probe(&native_dir).unwrap();
    assert_eq!(
        view.lookup(&at).unwrap(),
        TapeLookup::EmptyPrefix {
            digest: at.record_digest
        }
    );
    drop(authority);
    let replay =
        crate::library_log::store::replay(&root.path().join("library"), LOG, &view).unwrap();
    assert_eq!(replay.fold.highest_reference(at.tape), Some(0));
    let authority = open(&view);
    assert_eq!(
        authority
            .supersede_tail(value, AuditActor::System, SourceLayer::Layer4, &view)
            .unwrap()
            .receipt,
        receipt.receipt
    );
    drop(authority);
    fs::remove_file(native.path()).unwrap();
    assert_eq!(
        probe(&native_dir).unwrap().lookup(&at).unwrap(),
        TapeLookup::Contradiction
    );
    let replacement = Store::new_fixture(
        &native_dir,
        crate::tape_log::store::Header {
            tape_uuid: tape(1).uuid,
            block_size: 524288,
            scheme: None,
        },
    )
    .unwrap();
    replacement
        .acquire(true)
        .unwrap()
        .publish_empty_prefix()
        .unwrap();
    let replaced = probe(&native_dir).unwrap();
    let replay =
        crate::library_log::store::replay(&root.path().join("library"), LOG, &replaced).unwrap();
    assert!(replay.fold.contradicted(at.tape));
}
