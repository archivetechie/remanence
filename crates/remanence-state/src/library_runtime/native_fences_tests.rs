//! State-level source-pair tests: real lost FenceRaised publication, explicit recovery, runtime open.
use super::*;
use crate::authority::TapeAuthorityView;
use crate::tape_log::{snapshot::Snapshot, CommitCursor};
use remanence_library::durable_file::test_support::register;
use std::sync::atomic::AtomicU8;

#[path = "../../tests/support/library_fault.rs"]
#[allow(dead_code, clippy::duplicate_mod)]
// The brief requires this shared injector; its page-cache model is unused here.
mod durability;

const T: lib::Id = [0x71; 16];
const B: lib::Id = [0x72; 16];
const SEED: lib::Id = [0x70; 16];
const TIME: &str = "2026-11-11T00:00:00Z";

fn tape(uuid: lib::Id) -> lib::TapeRef {
    lib::TapeRef {
        uuid,
        incarnation: 1,
    }
}
fn raise(uuid: lib::Id, at: Option<lib::TapeAt>) -> lib::FenceRaised {
    lib::FenceRaised {
        kind: lib::FenceKind::Write,
        scope: lib::FenceScope::Tape(tape(uuid)),
        cause: lib::FenceCause::TransferError,
        evidence: None,
        at,
        operation: None,
        detail: lib::Bounded(vec![]),
    }
}

struct Fixture {
    runtime: Option<LibraryRuntime>,
    paths: StatePaths,
    lease: Arc<StateLockGuard>,
    uuid: lib::Id,
    _root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let p = root.path();
        let paths = StatePaths {
            config_path: p.join("config"),
            state_dir: p.join("state"),
            audit_dir: p.join("library"),
            journal_dir: p.join("native"),
            sqlite_path: p.join("catalog.sqlite"),
            tape_cache_dir: p.join("cache"),
            calibration_dir: p.join("calibration"),
        };
        let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
        let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
        let uuid = runtime.manifest().library_uuid();
        Self {
            runtime: Some(runtime),
            paths,
            lease,
            uuid,
            _root: root,
        }
    }
    fn rt(&self) -> &LibraryRuntime {
        self.runtime.as_ref().unwrap()
    }
    fn marker(&self) -> std::path::PathBuf {
        self.paths.audit_dir.join("library-log.fence-reconcile")
    }
    fn provision(&self, uuid: lib::Id) {
        self.rt()
            .append(|a, v| {
                a.reprovision(
                    lib::Reprovision {
                        tape: tape(uuid),
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
                        pool_source: None,
                        operation: None,
                        replaced_bot: None,
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
    }
    fn native(&self, uuid: lib::Id, reason: Option<SessionEndReason>) -> Snapshot {
        let authority = crate::TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            uuid,
            &self.paths.sqlite_path,
        )
        .unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        owner
            .publish_initialized(
                262144,
                None,
                CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
        let mut session = uuid;
        session[0] ^= 0x10;
        owner.open_session(session, TIME.into()).unwrap();
        if let Some(reason) = reason {
            owner
                .end_session(session, TIME.into(), reason, None, std::iter::empty())
                .unwrap();
        }
        let snapshot = owner.freeze().unwrap();
        self.rt().refresh_native(&snapshot).unwrap();
        snapshot.raw.clone()
    }
    fn failed() -> (Self, Snapshot) {
        let f = Self::new();
        f.provision(T);
        let native = f.native(T, Some(SessionEndReason::DriveError));
        (f, native)
    }
    fn lose(&self, uuid: lib::Id) {
        let registration = register(
            self.paths.audit_dir.clone(),
            Arc::new(durability::FaultDirectory(Arc::new(
                durability::Injection {
                    stage: AtomicU8::new(1),
                    after: false,
                },
            ))),
        );
        let result = self.rt().append(|a, v| {
            a.fence_raised(
                raise(uuid, None),
                lib::AuditActor::System,
                lib::SourceLayer::Layer5,
                v,
            )
        });
        assert!(
            matches!(
                result,
                Err(RuntimeError::Authority(AuthorityError::Store(
                    StoreError::Ambiguous(_)
                )))
            ),
            "{result:?}"
        );
        drop(registration);
        assert!(std::fs::read(self.marker())
            .unwrap()
            .chunks_exact(16)
            .any(|entry| entry == uuid));
    }
    fn clear(&mut self) {
        drop(self.runtime.take());
        let view = NativeTapeView::probe(
            &self.paths.journal_dir.join("tapes"),
            &self.paths.sqlite_path,
        )
        .unwrap();
        let mut log = lib::LibraryLog::open(
            &self.paths.audit_dir,
            self.uuid,
            Origin {
                host: "native-fences-test".into(),
                pid: std::process::id(),
                build: "test".into(),
            },
            Box::new(WallClock),
            Box::new(FreshUuids),
            true,
            &view,
        )
        .unwrap();
        log.clear_ambiguity(&view).unwrap();
    }
    fn reopen(&mut self) -> Result<(), RuntimeError> {
        drop(self.runtime.take());
        self.runtime = Some(LibraryRuntime::open(&self.paths, true, self.lease.clone())?);
        Ok(())
    }
    fn recover(&mut self) {
        self.clear();
        self.reopen().unwrap();
    }
    fn raises(&self) -> Vec<lib::Record> {
        self.rt()
            .test_source_records()
            .into_iter()
            .filter(|r| matches!(r.body_view(), lib::BodyView::FenceRaised(_)))
            .collect()
    }
    fn live(&self, uuid: lib::Id) -> Vec<crate::TapeIoFenceRecord> {
        self.rt()
            .reader()
            .unwrap()
            .tape_io_admission_conflicts(&uuid, None)
            .unwrap()
    }
    fn accepted_fence(&self, at: Option<lib::TapeAt>) -> String {
        let receipt = self
            .rt()
            .append(|a, v| {
                a.fence_raised(
                    raise(T, at),
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer5,
                    v,
                )
            })
            .unwrap();
        format!("library:{}", receipt.sequence)
    }
    fn release(&self, id: &str) {
        self.rt()
            .release_tape_fence(id, "operator examined the tail", lib::AuditActor::System)
            .unwrap();
    }
}

/// S1: one exact D4 record and its projected admission conflict, derived from the ending.
#[test]
fn s1_lost_fence_reconciles_exact_native_ending() {
    let (mut f, native) = Fixture::failed();
    let before = f.rt().test_source_records().len();
    f.lose(T);
    f.recover();
    let records = f.rt().test_source_records();
    assert_eq!(records.len(), before + 1);
    let record = records.last().unwrap();
    assert_eq!(record.envelope.actor, lib::AuditActor::System);
    assert_eq!(record.envelope.source_layer, lib::SourceLayer::Layer4);
    let sequence = native.summary().last_closed_sequence;
    assert_eq!(
        match record.body_view() {
            lib::BodyView::FenceRaised(fence) => fence,
            _ => panic!("expected fence"),
        },
        &lib::FenceRaised {
            kind: lib::FenceKind::Write,
            scope: lib::FenceScope::Tape(tape(T)),
            cause: lib::FenceCause::AuthorityAmbiguous,
            evidence: None,
            at: Some(lib::TapeAt {
                tape: tape(T),
                sequence,
                record_digest: native.record_evidence(sequence).unwrap().unwrap().digest
            }),
            operation: None,
            detail: lib::Bounded(vec![]),
        }
    );
    let live = f.live(T);
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].reason, "authority_ambiguous");
    assert_eq!(
        f.rt()
            .reader()
            .unwrap()
            .list_active_tape_io_fences()
            .unwrap()
            .len(),
        1
    );
    assert!(!f.marker().exists());
}

/// S2: both an active and a released fence cover a repeated owed reconciliation.
#[test]
fn s2_repeated_recovery_and_release_are_idempotent() {
    let (mut f, _) = Fixture::failed();
    f.lose(T);
    f.recover();
    let id = f.live(T)[0].quarantine_id.clone();
    f.lose(T);
    f.recover();
    assert_eq!(f.raises().len(), 1);
    f.release(&id);
    f.lose(T);
    f.recover();
    assert_eq!(f.raises().len(), 1);
    assert!(f.live(T).is_empty());
    assert!(!f.marker().exists());
}

/// S3: native failure alone does not authorize reconciliation.
#[test]
fn s3_no_lost_append_no_fence_on_two_opens() {
    let (mut f, _) = Fixture::failed();
    for _ in 0..2 {
        f.reopen().unwrap();
        assert!(f.raises().is_empty());
    }
}

/// S4a: an active tape fence is cover even without a native evidence reference.
#[test]
fn s4a_active_fence_without_at_covers() {
    let (mut f, _) = Fixture::failed();
    f.accepted_fence(None);
    f.lose(T);
    f.recover();
    assert_eq!(f.raises().len(), 1);
}

/// S4b: released evidence from before the failed session cannot cover it.
#[test]
fn s4b_released_fence_before_session_does_not_cover() {
    let (mut f, native) = Fixture::failed();
    let id = f.accepted_fence(Some(lib::TapeAt {
        tape: tape(T),
        sequence: 1,
        record_digest: native.record_evidence(1).unwrap().unwrap().digest,
    }));
    f.release(&id);
    f.lose(T);
    f.recover();
    assert_eq!(f.raises().len(), 2);
    assert_eq!(f.live(T)[0].reason, "authority_ambiguous");
}

/// S5: all specified skip cases are actually named by a lost append, never by helper writes.
#[test]
fn s5_named_ineligible_tapes_are_skipped() {
    for case in ["clean", "open", "retired", "unknown", "missing"] {
        let mut f = Fixture::new();
        f.provision(SEED);
        if case != "unknown" {
            f.provision(T);
        }
        if case != "missing" {
            f.native(
                T,
                match case {
                    "clean" => Some(SessionEndReason::CleanClose),
                    "open" => None,
                    _ => Some(SessionEndReason::DriveError),
                },
            );
        }
        if case == "retired" {
            f.rt()
                .append_identity(&[], |a, v, finalization| {
                    a.retire(
                        lib::Retire {
                            tape: tape(T),
                            reason: lib::RetireReason::OperatorRequested,
                            observed_tape_sequence: None,
                            assignment_generation: 1,
                            detail: None,
                        },
                        1,
                        finalization,
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer4,
                        v,
                    )
                })
                .unwrap();
        }
        // Lose a real publication first. The already-fenced store then loses T's
        // raise before the writer's liveness checks, exactly D2's refusal site.
        f.lose(SEED);
        let result = f.rt().append(|a, v| {
            a.fence_raised(
                raise(T, None),
                lib::AuditActor::System,
                lib::SourceLayer::Layer5,
                v,
            )
        });
        assert!(
            matches!(
                result,
                Err(RuntimeError::Authority(AuthorityError::Store(
                    StoreError::Fenced
                )))
            ),
            "{case}: {result:?}"
        );
        assert!(std::fs::read(f.marker())
            .unwrap()
            .chunks_exact(16)
            .any(|entry| entry == T));
        f.recover();
        assert!(f.raises().is_empty(), "{case}");
        assert!(!f.marker().exists());
        eprintln!("D3 skip fixture: {case}=1, seed no-native=1");
    }
}

/// S6: all reasons crossed with empty and nonempty tail evidence, independently of logs.
#[test]
fn s6_ending_predicate_covers_every_reason_and_tail() {
    use SessionEndReason::*;
    let (_f, native) = Fixture::failed();
    let mut ending = native.summary().last_session_end.clone().unwrap();
    for reason in [
        CleanClose,
        Abort,
        FailedAppend,
        ClientStall,
        DriveError,
        GracefulShutdown,
        ClientDisconnected,
        CrashRecovered,
        CloseoutFailed,
    ] {
        for (files, blocks) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            ending.reason = reason;
            ending.tail_file_count = files;
            ending.possible_tail_blocks = blocks;
            assert_eq!(
                failed_ending(&ending),
                matches!(reason, FailedAppend | DriveError | CloseoutFailed)
                    || files != 0
                    || blocks != 0,
                "{reason:?} {files} {blocks}"
            );
        }
    }
}

/// S7: a failed open after its append retains the trigger and repeats without duplication.
#[test]
fn s7_cut_after_append_retries_once() {
    let (mut f, _) = Fixture::failed();
    f.lose(T);
    f.clear();
    CUT.set(2);
    let result = f.reopen();
    CUT.set(0);
    assert!(result.is_err());
    assert!(f.runtime.is_none());
    assert!(f.marker().exists());
    f.reopen().unwrap();
    assert_eq!(f.raises().len(), 1);
    assert!(!f.marker().exists());
}

/// S8: no runtime or fence escapes the before-append cut; malformed entries fail closed.
#[test]
fn s8_cut_before_append_and_malformed_marker_refuse_open() {
    let (mut f, _) = Fixture::failed();
    f.lose(T);
    f.clear();
    CUT.set(1);
    let result = f.reopen();
    CUT.set(0);
    assert!(result.is_err());
    assert!(f.runtime.is_none());
    assert!(f.marker().exists());
    let view =
        NativeTapeView::probe(&f.paths.journal_dir.join("tapes"), &f.paths.sqlite_path).unwrap();
    let replay = lib::store::replay(&f.paths.audit_dir, f.uuid, &view).unwrap();
    assert!(replay.fold.active_fence(2).is_none());
    assert_eq!(replay.last_sequence, 1, "no record appended before refusal");
    f.reopen().unwrap();
    assert_eq!(f.raises().len(), 1);
    drop(f.runtime.take());
    std::fs::write(f.marker(), [0u8; 17]).unwrap();
    assert!(f.reopen().is_err());
    assert!(f.runtime.is_none());
    assert_eq!(std::fs::read(f.marker()).unwrap().len(), 17);
}

/// S10: the production pin's CleanClose/zero-tail shape is not eligible, even when named.
#[test]
fn s10_clean_close_zero_tail_is_not_fenced() {
    let mut f = Fixture::new();
    f.provision(T);
    let native = f.native(T, Some(SessionEndReason::CleanClose));
    let ending = native.summary().last_session_end.as_ref().unwrap();
    assert_eq!(
        (
            ending.reason,
            ending.tail_file_count,
            ending.possible_tail_blocks
        ),
        (SessionEndReason::CleanClose, 0, 0)
    );
    f.lose(T);
    f.recover();
    assert!(f.raises().is_empty());
    assert!(f.live(T).is_empty());
}

/// S11: an unlisted daemon failure is not swept into the named tape's reconciliation.
#[test]
fn s11_only_lost_tape_is_fenced() {
    let (mut f, _) = Fixture::failed();
    f.provision(B);
    f.native(B, Some(SessionEndReason::FailedAppend));
    f.lose(T);
    f.recover();
    assert_eq!(f.raises().len(), 1);
    assert_eq!(f.live(T).len(), 1);
    assert!(f.live(B).is_empty());
}
