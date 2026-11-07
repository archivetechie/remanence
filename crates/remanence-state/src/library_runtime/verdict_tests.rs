//! `record_verification_verdict`: the tape identity and native reference are resolved
//! under the dispatch lease, and the projection takes its state from the outcome.
use super::*;
use crate::authority::{TapeAuthorityView, TapeAuthorityWriter};

const ALL_BITS: u32 = 0x7f;

struct Fixture {
    paths: StatePaths,
    runtime: LibraryRuntime,
    _lease: Arc<StateLockGuard>,
    _root: tempfile::TempDir,
}

fn open() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let paths = StatePaths {
        config_path: root.path().join("config"),
        state_dir: root.path().join("state"),
        audit_dir: root.path().join("library"),
        journal_dir: root.path().join("native"),
        sqlite_path: root.path().join("catalog.sqlite"),
        tape_cache_dir: root.path().join("cache"),
        calibration_dir: root.path().join("calibration"),
    };
    let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
    let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
    Fixture {
        paths,
        runtime,
        _lease: lease,
        _root: root,
    }
}

fn provision(runtime: &LibraryRuntime, id: lib::Id) {
    runtime
        .append(|a, v| {
            a.reprovision(
                lib::Reprovision {
                    pool_source: None,
                    operation: None,
                    replaced_bot: None,
                    tape: lib::TapeRef {
                        uuid: id,
                        incarnation: 1,
                    },
                    predecessor: None,
                    mode: lib::ProvisionMode::Initialize,
                    label: None,
                    label_provenance: None,
                    parity_scheme: None,
                    medium: lib::MediumSpec {
                        family: lib::MediumFamily::Lto(lib::LtoGen::Lto9),
                        usable_logical_bytes: 18_000_000_000_000,
                        record_size: 262144,
                        rerecord: true,
                        lifecycle_ops: true,
                        source: lib::GenerationSource::Operator,
                    },
                    pool: None,
                    endpoint: None,
                    adoption: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
}

/// Give the tape a native log with one closed record, so a native reference exists.
fn native_log(
    paths: &StatePaths,
    runtime: &LibraryRuntime,
    id: lib::Id,
) -> crate::TapeAuthorityLease {
    let mut owner = crate::TapeAuthority::open_fixture(paths.journal_dir.join("tapes"), id)
        .unwrap()
        .acquire_exclusive()
        .unwrap();
    owner
        .publish_report(&(&crate::checkpoint::tests::record(id)).into())
        .unwrap();
    runtime.refresh_native(&owner.freeze().unwrap()).unwrap();
    owner
}

fn facts(outcome: lib::VerificationOutcome) -> VerdictFacts {
    VerdictFacts {
        native_reference: None,
        outcome,
        unprotected: None,
        modes: ALL_BITS,
        edition_digest: Some([7; 32]),
    }
}

/// The verdict records of the library log, in sequence order.
fn verdicts(runtime: &LibraryRuntime) -> Vec<lib::VerificationVerdict> {
    runtime
        .test_source_records()
        .into_iter()
        .filter_map(|record| match record.body_view() {
            lib::BodyView::VerificationVerdict(v) => Some(v.clone()),
            _ => None,
        })
        .collect()
}

/// D-W-7, producer side: a tape whose native reference was captured records that reference,
/// never `at: None`.
#[test]
fn verdict_of_a_tape_with_a_native_log_names_its_current_native_reference() {
    let f = open();
    let id = [0x61; 16];
    provision(&f.runtime, id);
    let _owner = native_log(&f.paths, &f.runtime, id);
    let reference = f
        .runtime
        .current_native_reference(id)
        .unwrap()
        .expect("the native log has a closed record");
    let recorded = f
        .runtime
        .record_verification_verdict(
            [0xa1; 16],
            id,
            VerdictFacts {
                native_reference: Some(reference.clone()),
                ..facts(lib::VerificationOutcome::VerifiedComplete)
            },
            lib::AuditActor::System,
        )
        .unwrap();
    assert_eq!(recorded.at.as_ref(), Some(&reference));
    assert_eq!(recorded.tape, reference.tape);
    assert_eq!(recorded.tape, f.runtime.tape_state(id).unwrap().tape);
    assert_eq!(recorded.damaged.0.len(), 0);
    assert_eq!(recorded.progress, None);
    // The log holds exactly what was returned, and the fold accepted it.
    assert_eq!(verdicts(&f.runtime), vec![recorded]);
}

/// D-2b-3: the wrapper records the reference it is given, never one it reads itself. A native
/// write after the capture (here, a second closed record) moves the tape's current reference, and
/// the verdict still names the captured one: it claims the position the verification read.
#[test]
fn verdict_records_the_captured_reference_not_a_later_one() {
    let f = open();
    let id = [0x64; 16];
    provision(&f.runtime, id);
    let mut owner = native_log(&f.paths, &f.runtime, id);
    let captured = f
        .runtime
        .current_native_reference(id)
        .unwrap()
        .expect("the native log has a closed record");
    // The write in the window: a later closed record on the same tape log.
    owner
        .publish_report(&(&crate::checkpoint::tests::second_record(id)).into())
        .unwrap();
    f.runtime.refresh_native(&owner.freeze().unwrap()).unwrap();
    let later = f
        .runtime
        .current_native_reference(id)
        .unwrap()
        .expect("the log still has a closed record");
    assert!(later.sequence > captured.sequence, "{later:?} {captured:?}");
    let recorded = f
        .runtime
        .record_verification_verdict(
            [0xa4; 16],
            id,
            VerdictFacts {
                native_reference: Some(captured.clone()),
                ..facts(lib::VerificationOutcome::VerifiedComplete)
            },
            lib::AuditActor::System,
        )
        .unwrap();
    assert_eq!(recorded.at, Some(captured));
    assert_eq!(verdicts(&f.runtime), vec![recorded]);
}

/// D-W-7, producer side: `at: None` when the caller captured no native reference, and the fold accepts it.
#[test]
fn verdict_of_a_tape_with_no_native_log_records_no_reference() {
    let f = open();
    let id = [0x62; 16];
    provision(&f.runtime, id);
    assert_eq!(f.runtime.current_native_reference(id).unwrap(), None);
    let recorded = f
        .runtime
        .record_verification_verdict(
            [0xa2; 16],
            id,
            facts(lib::VerificationOutcome::VerifiedComplete),
            lib::AuditActor::System,
        )
        .unwrap();
    assert_eq!(recorded.at, None);
    assert_eq!(recorded.tape, f.runtime.tape_state(id).unwrap().tape);
    assert_eq!(verdicts(&f.runtime), vec![recorded]);
}

/// A verdict names a tape the library log knows; nothing is appended for one it does not.
#[test]
fn verdict_of_an_unknown_tape_is_refused_before_any_append() {
    let f = open();
    let error = f
        .runtime
        .record_verification_verdict(
            [0xa3; 16],
            [0x63; 16],
            facts(lib::VerificationOutcome::VerifiedComplete),
            lib::AuditActor::System,
        )
        .unwrap_err();
    assert!(error.to_string().contains("library log lacks"), "{error}");
    assert!(verdicts(&f.runtime).is_empty());
}

/// T3: the projected state is the outcome (2, 3, 4), whatever `damaged` and `unprotected` say.
/// `VerifiedComplete` with an unprotected tail is 2 (the old inference gave 4), and
/// `VerifiedDegraded` or `RecoveryRequired` with no ranges are 3 and 4 (it gave 2).
#[test]
fn projected_verification_state_follows_the_outcome() {
    let f = open();
    for (n, (outcome, unprotected, state)) in [
        (lib::VerificationOutcome::VerifiedComplete, None, 2),
        (
            lib::VerificationOutcome::VerifiedComplete,
            Some(lib::Unprotected { w: 1, t: 2 }),
            2,
        ),
        (lib::VerificationOutcome::VerifiedDegraded, None, 3),
        (
            lib::VerificationOutcome::VerifiedDegraded,
            Some(lib::Unprotected { w: 1, t: 2 }),
            3,
        ),
        (lib::VerificationOutcome::RecoveryRequired, None, 4),
    ]
    .into_iter()
    .enumerate()
    {
        let id = [0x70 + n as u8; 16];
        provision(&f.runtime, id);
        let digest =
            (outcome != lib::VerificationOutcome::RecoveryRequired).then_some([n as u8; 32]);
        f.runtime
            .record_verification_verdict(
                [0xb0 + n as u8; 16],
                id,
                VerdictFacts {
                    unprotected: unprotected.clone(),
                    edition_digest: digest,
                    ..facts(outcome)
                },
                lib::AuditActor::System,
            )
            .unwrap();
        let row = f.runtime.reader().unwrap().get_tape(&id).unwrap().unwrap();
        assert_eq!(
            row.last_verification_state,
            Some(state),
            "{outcome:?} {unprotected:?}"
        );
        assert!(row.last_verified_at_utc.is_some());
        assert_eq!(row.last_verified_edition_digest, digest.map(|d| d.to_vec()));
    }
    // Replay from the source records rebuilds the same projection.
    let before: Vec<_> = (0..5)
        .map(|n| {
            f.runtime
                .reader()
                .unwrap()
                .get_tape(&[0x70 + n; 16])
                .unwrap()
                .unwrap()
        })
        .collect();
    f.runtime
        .rebuild_projection(
            &mut crate::projector::open(&f.paths.sqlite_path)
                .unwrap()
                .write_handle(),
        )
        .unwrap();
    for (n, before) in before.iter().enumerate() {
        let after = f
            .runtime
            .reader()
            .unwrap()
            .get_tape(&[0x70 + n as u8; 16])
            .unwrap()
            .unwrap();
        assert_eq!(
            after.last_verification_state,
            before.last_verification_state
        );
        assert_eq!(after.last_verified_at_utc, before.last_verified_at_utc);
        assert_eq!(
            after.last_verified_edition_digest,
            before.last_verified_edition_digest
        );
    }
}
