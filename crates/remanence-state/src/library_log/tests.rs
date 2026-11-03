//! Contract tests use the real codec/store, fixed origins and a typed native proof model.
use super::store::*;
use super::*;
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

pub(super) const NOW: u64 = 1_793_664_000_000_000_000;
pub(super) const LOG: Id = [0xb1; 16];
#[derive(Clone)]
pub(super) struct FixedClock(pub Arc<AtomicU64>);
impl Clock for FixedClock {
    fn now(&self) -> u64 {
        self.0.fetch_add(1_000_000_000, Ordering::SeqCst)
    }
}
pub(super) struct Ids;
impl UuidSource for Ids {
    fn next(&self, n: u64) -> Id {
        let mut id = [0; 16];
        id[0] = 0xc0;
        id[8..].copy_from_slice(&n.to_be_bytes());
        id
    }
}
#[derive(Default)]
pub(super) struct View(pub BTreeMap<(Id, u64), TapeLookup>);
impl TapeView for View {
    fn lookup(&self, a: &TapeAt) -> TapeLookup {
        self.0
            .get(&(a.tape.uuid, a.sequence))
            .cloned()
            .unwrap_or(TapeLookup::NotYet)
    }
}
#[derive(Default)]
pub(super) struct History(pub BTreeMap<u64, Record>);
impl LibraryHistory for History {
    fn lookup_fence(&self, sequence: u64) -> Result<Option<Record>, FoldError> {
        Ok(self.0.get(&sequence).cloned())
    }
    fn lookup(&self, reference: DecisionRef) -> Result<Option<Record>, FoldError> {
        Ok(self
            .0
            .get(&reference.sequence)
            .filter(|r| r.envelope.record_uuid == reference.record_uuid)
            .cloned())
    }
}
pub(super) struct Present;
impl TapeView for Present {
    fn lookup(&self, a: &TapeAt) -> TapeLookup {
        TapeLookup::Present {
            kind: crate::tape_log::Tag::FinalizationPrepared,
            digest: a.record_digest,
        }
    }
}
pub(super) fn tape(n: u8) -> TapeRef {
    TapeRef {
        uuid: [n; 16],
        incarnation: 1,
    }
}
pub(super) fn at(n: u8, seq: u64) -> TapeAt {
    TapeAt {
        tape: tape(n),
        sequence: seq,
        record_digest: [seq as u8; 32],
    }
}
pub(super) fn endpoint(l: &str) -> DriveKey {
    DriveKey::Bay(Bay {
        library_serial: l.into(),
        bay: 1,
    })
}
pub(super) fn evidence() -> CompletionEvidence {
    CompletionEvidence {
        dispatch: Dispatch::Issued,
        completion: Completion::Known,
        scsi_status: Some(0),
        host_status: Some(0),
        driver_status: Some(0),
        sense: Some(Blob(vec![0x70, 0xff, 0x02])),
        residual: Some(0),
    }
}
pub(super) fn provision(n: u8, label: Option<&str>) -> Body {
    Body::Reprovision(Reprovision {
        tape: tape(n),
        predecessor: None,
        mode: ProvisionMode::Initialize,
        label: label.map(Into::into),
        label_provenance: label.map(|_| LabelProvenance::Changer),
        parity_scheme: None,
        medium: MediumSpec {
            family: MediumFamily::Lto(LtoGen::Lto9),
            usable_logical_bytes: 18_000_000_000_000,
            record_size: 1048576,
            rerecord: false,
            lifecycle_ops: false,
            source: GenerationSource::Measured,
        },
        pool: None,
        endpoint: None,
        adoption: None,
    })
}
pub(super) fn diagnostic() -> Body {
    Body::Diagnostic(Diagnostic {
        event: DiagnosticEvent::RequestReceived,
        subject_kind: "fixture".into(),
        subject_id: None,
        detail: Bounded(vec![]),
    })
}
pub(super) fn record(body: Body, n: u64) -> Record {
    Record {
        envelope: Envelope {
            record_uuid: Ids.next(n),
            timestamp_unix_ns: NOW + n * 1_000_000_000,
            host_id: "fixture-host".into(),
            process_id: 4242,
            software_build: "remanence-fixture".into(),
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
        },
        body,
    }
}
pub(super) fn receipt(r: &Record, n: u64) -> Receipt {
    Receipt {
        sequence: n,
        record_uuid: r.envelope.record_uuid,
        record_hash: [0; 32],
        synced: true,
    }
}
pub(super) fn feed(
    f: &mut LibraryFold,
    b: Body,
    v: &dyn TapeView,
) -> Result<fold::Validated, FoldError> {
    let n = f.received_through + 1;
    let r = record(b, n);
    let receipt = receipt(&r, n);
    let mut replay = f.clone();
    let mut live = f.clone();
    let result = super::differential_tests::step(
        &mut live,
        &mut replay,
        r.clone(),
        receipt.clone(),
        v,
        &History::default(),
    );
    let applied = f.apply(n, r, receipt, v, &History::default());
    match &applied {
        Ok(_) => {
            result.unwrap();
            f.assert_same_state(&live);
        }
        Err(error) => {
            if let Err(replay_error) = result {
                assert_eq!(error, &replay_error);
            }
        }
    }
    applied
}
pub(super) fn open(path: &Path, view: &dyn TapeView) -> (LibraryLog, FixedClock) {
    let c = FixedClock(Arc::new(AtomicU64::new(NOW + 1_000_000_000)));
    let l = LibraryLog::open(
        path,
        LOG,
        Origin {
            host: "fixture-host".into(),
            pid: 4242,
            build: "remanence-fixture".into(),
        },
        Box::new(c.clone()),
        Box::new(Ids),
        false,
        view,
    )
    .unwrap();
    (l, c)
}
pub(super) fn put(l: &mut LibraryLog, b: Body) -> Receipt {
    l.append(
        b,
        AuditActor::User("operator".into()),
        SourceLayer::Layer4,
        &Present,
    )
    .unwrap()
}
pub(super) fn reference(r: &Receipt) -> DecisionRef {
    DecisionRef {
        sequence: r.sequence,
        record_uuid: r.record_uuid,
    }
}
pub(super) fn pool(n: u8, g: u64) -> Body {
    Body::AssignPool(AssignPool {
        tape: tape(n),
        pool: Some("pool-a".into()),
        change: GenerationChange {
            expected: g,
            next: g + 1,
        },
        source: AssignmentSource::Operator,
        configuration: None,
    })
}
pub(super) fn progress(n: u32) -> Body {
    Body::VerificationProgress(VerificationProgress {
        operation: [7; 16],
        at: at(1, 1),
        plan_digest: [3; 32],
        modes: 1,
        parts_done: n,
        next: VerificationCursor {
            partition: 0,
            file: n as u64,
            lba: n as u64,
            ordinal: n as u64,
            part: n,
        },
        completed_digest: [4; 32],
    })
}
pub(super) fn verdict() -> Body {
    Body::VerificationVerdict(VerificationVerdict {
        operation: [7; 16],
        at: at(1, 1),
        damaged: Bounded(vec![]),
        unprotected: None,
        modes: 1,
        edition_digest: None,
        progress: None,
    })
}
pub(super) fn fence(n: u8) -> Body {
    Body::FenceRaised(FenceRaised {
        kind: FenceKind::Write,
        scope: FenceScope::Tape(tape(n)),
        cause: FenceCause::TransferError,
        evidence: Some(evidence()),
        at: Some(at(n, 999)),
        operation: None,
    })
}
pub(super) fn intent(attempt: u32) -> Body {
    Body::MotionIntent(MotionIntent {
        command: [attempt as u8; 16],
        parent_operation: Some([8; 16]),
        motion: Motion::Load(Load {
            endpoint: endpoint("LIBA0001"),
            attempt,
        }),
        tape: Some(tape(1)),
        inventory_generation: None,
        readiness_generation: None,
    })
}
fn initialized() -> LibraryFold {
    let mut f = LibraryFold::default();
    feed(&mut f, provision(1, Some("A")), &Present).unwrap();
    feed(&mut f, provision(2, Some("B")), &Present).unwrap();
    f
}
/// Matching digest with the wrong native kind is a typed refusal, never pending.
#[test]
fn counterpart_kind_is_checked() {
    let mut f = initialized();
    let b = Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
        at: at(1, 1),
        operation: [8; 16],
        actor_fingerprint: "actor".into(),
        idempotency_key: [8; 16],
        request_fingerprint: [1; 32],
        assignment_generation: 1,
    });
    let view = View(BTreeMap::from([(
        (tape(1).uuid, 1),
        TapeLookup::Present {
            kind: crate::tape_log::Tag::SessionOpened,
            digest: [1; 32],
        },
    )]));
    assert_eq!(
        feed(&mut f, b.clone(), &view).unwrap_err(),
        FoldError::CounterpartKind
    );
    let r = record(b, 3);
    assert_eq!(
        f.receive(3, r.clone(), receipt(&r, 3), &view, &History::default()),
        Err(FoldError::CounterpartKind)
    );
    assert!(f.pending.is_empty());
}
/// A pending verification blocks assignment, but fences and unrelated tapes proceed.
#[test]
fn pending_blocks_only_its_scope_in_order() {
    let mut f = initialized();
    for b in [verdict(), pool(1, 1), fence(1), pool(2, 1)] {
        let n = f.received_through + 1;
        let r = record(b, n);
        f.receive(
            n,
            r.clone(),
            receipt(&r, n),
            &View::default(),
            &History::default(),
        )
        .unwrap();
    }
    assert_eq!(f.pending.len(), 3); // includes the already-applied fence validation
    assert_eq!(f.open_fences(), 1);
    assert_eq!(f.tapes[&tape(1).uuid].assignment_generation, 1);
    assert_eq!(f.tapes[&tape(2).uuid].assignment_generation, 2);
    assert_eq!(f.applied_through, 2);
    f.resolve(&Present, &History::default()).unwrap();
    assert!(f.pending.is_empty());
    assert_eq!(f.applied_through, 6);
    assert_eq!(f.tapes[&tape(1).uuid].assignment_generation, 2);
}
/// Live append cannot promise authority against unavailable native proof.
#[test]
fn append_refuses_unresolved_tape_reference() {
    let dir = tempfile::tempdir().unwrap();
    let (mut l, _) = open(dir.path(), &Present);
    put(&mut l, provision(1, None));
    assert!(matches!(
        l.append(
            verdict(),
            AuditActor::System,
            SourceLayer::Layer4,
            &View::default()
        ),
        Err(StoreError::Fold(FoldError::Unresolved))
    ));
    assert_eq!(l.state().last_sequence, 1);
}
/// Progress validates coverage/cursors and cannot confer a verdict.
#[test]
fn verification_progress_is_not_verdict() {
    let mut f = initialized();
    feed(&mut f, progress(1), &Present).unwrap();
    assert_eq!(
        feed(&mut f, verdict(), &Present).unwrap_err(),
        FoldError::VerdictWithoutCoverage
    );
    let mut b = progress(2);
    if let Body::VerificationProgress(v) = &mut b {
        v.next.part = 0;
    }
    assert_eq!(
        feed(&mut f, b, &Present).unwrap_err(),
        FoldError::ImpossibleCursor
    );
    let mut b = progress(2);
    if let Body::VerificationProgress(v) = &mut b {
        v.plan_digest = [9; 32];
    }
    assert_eq!(
        feed(&mut f, b, &Present).unwrap_err(),
        FoldError::CoverageChanged
    );
    assert_eq!(
        feed(&mut f, progress(0), &Present).unwrap_err(),
        FoldError::ProgressRegressed
    );
}
/// Label transfer is one generation-CAS record on both live identities.
#[test]
fn library_label_transfer_generation_cas() {
    let mut f = initialized();
    let mut b = Body::Label(Label {
        tape: tape(2),
        label: Some("A".into()),
        provenance: LabelProvenance::Operator,
        change: GenerationChange {
            expected: 1,
            next: 2,
        },
        prior_holder: None,
        observed_bot_uuid: Some(tape(2).uuid),
    });
    assert_eq!(
        feed(&mut f, b.clone(), &Present).unwrap_err(),
        FoldError::LabelHeld
    );
    if let Body::Label(v) = &mut b {
        v.prior_holder = Some(PriorHolder {
            tape: tape(1),
            change: GenerationChange {
                expected: 1,
                next: 2,
            },
        });
    }
    feed(&mut f, b, &Present).unwrap();
    assert!(f.tapes[&tape(1).uuid].label.is_none());
    assert_eq!(f.tapes[&tape(2).uuid].label, Some("A".into()));
    assert_eq!(
        feed(&mut f, pool(1, 0), &Present).unwrap_err(),
        FoldError::GenerationMismatch
    );
}
/// Retired identities remain unusable; operator detail has no fold meaning.
#[test]
fn retired_identity_is_never_resurrected() {
    let mut f = initialized();
    feed(
        &mut f,
        Body::Retire(Retire {
            tape: tape(1),
            reason: RetireReason::OperatorRequested,
            observed_tape_sequence: Some(999),
            assignment_generation: 1,
            detail: Some("  free text  ".into()),
        }),
        &Present,
    )
    .unwrap();
    assert_eq!(
        feed(&mut f, pool(1, 1), &Present).unwrap_err(),
        FoldError::RetiredIdentity
    );
    feed(&mut f, diagnostic(), &Present).unwrap();
    feed(&mut f, provision(3, Some("A")), &Present).unwrap();
}
/// Releases and outcomes must name earlier, correctly typed open causes.
#[test]
fn release_before_raise_is_refused() {
    let mut f = initialized();
    assert_eq!(
        feed(
            &mut f,
            Body::FenceReleased(FenceReleased {
                fence: 1,
                evidence: "proof".into(),
                forced: true
            }),
            &Present
        )
        .unwrap_err(),
        FoldError::WrongCounterpart
    );
    assert_eq!(
        feed(
            &mut f,
            Body::FenceReleased(FenceReleased {
                fence: 3,
                evidence: "proof".into(),
                forced: true
            }),
            &Present
        )
        .unwrap_err(),
        FoldError::ForwardReference
    );
}
/// A missing resolved intent is not resurrected by an outcome.
#[test]
fn outcome_before_intent_is_refused() {
    let mut f = initialized();
    let b = Body::MotionOutcome(MotionOutcome {
        intent: DecisionRef {
            sequence: 1,
            record_uuid: [0; 16],
        },
        result: MotionResult::Unknown,
        evidence: evidence(),
        confirmation: None,
    });
    assert_eq!(
        feed(&mut f, b, &Present).unwrap_err(),
        FoldError::OutcomeWithoutOpenIntent
    );
}
/// LOAD accounting uses intent attempts, independently of readiness records.
#[test]
fn load_attempt_count_lives_on_intents() {
    let mut f = initialized();
    feed(&mut f, intent(1), &Present).unwrap();
    assert_eq!(
        feed(&mut f, intent(3), &Present).unwrap_err(),
        FoldError::LoadAttempt
    );
    feed(&mut f, intent(2), &Present).unwrap();
    feed(&mut f, intent(3), &Present).unwrap();
    assert!(f.inventory_unknown(&fold::Scope::Library("LIBA0001".into())));
    assert!(!f.inventory_unknown(&fold::Scope::Library("LIBB0002".into())));
}
/// Exact decision retries return the first receipt; changed bytes cannot reuse a generation.
#[test]
fn duplicate_identical_is_noop_conflicting_is_refused() {
    let mut f = initialized();
    feed(&mut f, pool(1, 1), &Present).unwrap();
    assert!(matches!(
        feed(&mut f, pool(1, 1), &Present).unwrap(),
        fold::Validated::Duplicate(_)
    ));
    let mut b = pool(1, 1);
    if let Body::AssignPool(v) = &mut b {
        v.pool = None;
    }
    assert_eq!(
        feed(&mut f, b, &Present).unwrap_err(),
        FoldError::ConflictingDuplicate
    );
}
/// Native digest contradiction is scoped while later unrelated decisions remain available.
#[test]
fn tape_digest_mismatch_is_scoped_contradiction() {
    let mut f = initialized();
    let v = View(BTreeMap::from([(
        (tape(1).uuid, 1),
        TapeLookup::Present {
            kind: crate::tape_log::Tag::SessionOpened,
            digest: [9; 32],
        },
    )]));
    let r = record(verdict(), 3);
    f.receive(3, r.clone(), receipt(&r, 3), &v, &History::default())
        .unwrap();
    assert_eq!(f.contradictions.len(), 1);
    feed(&mut f, pool(2, 1), &Present).unwrap();
}
/// Rotation follows later UTC dates; regression appends to the newest segment.
#[test]
fn library_chain_rotation_clock_regression() {
    let d = tempfile::tempdir().unwrap();
    let (mut l, c) = open(d.path(), &Present);
    put(&mut l, diagnostic());
    c.0.store(NOW + 86_400_000_000_000, Ordering::SeqCst);
    put(&mut l, diagnostic());
    c.0.store(NOW, Ordering::SeqCst);
    put(&mut l, diagnostic());
    c.0.store(NOW + 2 * 86_400_000_000_000, Ordering::SeqCst);
    put(&mut l, diagnostic());
    assert_eq!(l.state().segments.len(), 3);
    drop(l);
    let r = replay(d.path(), LOG, &Present).unwrap();
    assert!(r.refusal.is_none());
    assert_eq!(r.last_sequence, 4);
    assert_eq!(r.segments.len(), 3);
}
/// No reserved native fact can be represented by a library record tag.
#[test]
fn tape_owned_fact_cannot_be_expressed() {
    use crate::log_frame::Family;
    assert_eq!(
        codec::LibraryFamily::validate(1, 30),
        Err(codec::Fault::UnknownTag(30))
    );
    use crate::log_frame::cbor::Wire;
    assert!(SessionKind::get(&mut &[0u8][..]).is_err());
    assert!(RecoveryAction::get(&mut &[0x82, 0, 0xf6][..]).is_err());
    assert!(DiagnosticEvent::get(&mut &[20u8][..]).is_err());
}
/// The reserved unprotected pair round-trips and waits without any public constructor.
#[test]
fn unprotected_pair_reserved_without_emitter() {
    let mut f = initialized();
    let b = Body::AcceptUnprotectedFinalization(AcceptUnprotectedFinalization {
        at: at(1, 999),
        operation: [2; 16],
        w: 1,
        t: 2,
        reason: "fixture only".into(),
    });
    let r = record(b, 3);
    let (bytes, _) = codec::encode(3, [0; 32], &r).unwrap();
    let mut d = codec::Decoder::at(bytes.as_slice(), 3, 0);
    assert_eq!(d.next_frame().unwrap().unwrap().record, r);
    f.receive(
        3,
        r.clone(),
        receipt(&r, 3),
        &View::default(),
        &History::default(),
    )
    .unwrap();
    assert_eq!(f.pending.len(), 1);
}
/// Manual finalization uses the current assignment generation before checking native proof.
#[test]
fn manual_acceptance_with_stale_generation_is_refused() {
    let mut f = initialized();
    let b = Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
        at: at(1, 1),
        operation: [5; 16],
        actor_fingerprint: "operator".into(),
        idempotency_key: [5; 16],
        request_fingerprint: [6; 32],
        assignment_generation: 0,
    });
    assert_eq!(
        feed(&mut f, b, &Present).unwrap_err(),
        FoldError::GenerationMismatch
    );
}
/// Config generations and the canonical definitions digest are both binding.
#[test]
fn config_digest_and_generation() {
    let mut f = initialized();
    let defs = Definitions {
        pools: Bounded(vec![]),
        rules: Bounded(vec![]),
        cleaning_prefixes: Bounded(vec![]),
    };
    let mut c = ConfigurationAccepted {
        generation: 1,
        config_digest: [0; 32],
        definitions: defs,
    };
    assert_eq!(
        feed(&mut f, Body::ConfigurationAccepted(c.clone()), &Present).unwrap_err(),
        FoldError::ConfigDigest
    );
    c.config_digest = codec::configuration_digest(&c.definitions).unwrap();
    feed(&mut f, Body::ConfigurationAccepted(c), &Present).unwrap();
}
/// Every prefix cut is checked against the production streaming boundary classifier.
#[test]
fn library_log_every_byte_cut_preserves_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let (mut l, c) = open(dir.path(), &Present);
    put(&mut l, diagnostic());
    put(&mut l, diagnostic());
    c.0.store(NOW + 86_400_000_000_000, Ordering::SeqCst);
    put(&mut l, diagnostic());
    put(&mut l, diagnostic());
    let segments = l.state().segments.clone();
    drop(l);
    for (i, s) in segments.iter().enumerate() {
        let original = std::fs::read(dir.path().join(&s.name)).unwrap();
        let mut decoder = codec::Decoder::at(
            &original[84..],
            s.header.as_ref().unwrap().first_sequence,
            84,
        );
        let mut ends = vec![84];
        while let Some(f) = decoder.next_frame().unwrap() {
            ends.push(f.end);
        }
        for cut in 0..original.len() {
            std::fs::write(dir.path().join(&s.name), &original[..cut]).unwrap();
            let r = replay(dir.path(), LOG, &Present).unwrap();
            let expected = ends
                .iter()
                .copied()
                .filter(|e| *e <= cut as u64)
                .max()
                .unwrap_or(0);
            assert_eq!(r.segments[i].through, expected, "segment {i} cut {cut}");
            if cut < 84 || i == 0 || expected == 84 {
                let refusal = r.refusal.as_ref().expect("damaged prefix must stop");
                let (segment, offset, sequence, fault) = if cut < 84 {
                    (s.name.as_str(), 0, None, codec::Fault::Header)
                } else if expected == 84 {
                    (
                        s.name.as_str(),
                        84,
                        Some(s.header.as_ref().unwrap().first_sequence),
                        codec::Fault::EmptySegment,
                    )
                } else if cut as u64 == expected {
                    (
                        segments[i + 1].name.as_str(),
                        0,
                        Some(segments[i + 1].header.as_ref().unwrap().first_sequence),
                        codec::Fault::SegmentSequence,
                    )
                } else {
                    (
                        s.name.as_str(),
                        expected,
                        Some(s.header.as_ref().unwrap().first_sequence + 1),
                        codec::Fault::InteriorTorn,
                    )
                };
                assert_eq!(
                    (
                        refusal.segment.as_str(),
                        refusal.offset,
                        refusal.sequence,
                        &refusal.fault
                    ),
                    (segment, offset, sequence, &RefusalKind::Bytes(fault)),
                    "{i} {cut}"
                );
            } else {
                assert!(r.refusal.is_none(), "{i} {cut} {:?}", r.refusal);
            }
        }
        std::fs::write(dir.path().join(&s.name), original).unwrap();
    }
}
#[path = "../../tests/support/library_fault.rs"]
mod durability;
/// Unknown outcomes at every host durability boundary fence, retaining landed claims.
#[test]
fn library_log_ambiguous_fsync_never_rolls_back_claim() {
    use remanence_library::durable_file::test_support::register;
    use std::sync::atomic::AtomicU8;
    for existing in [false, true] {
        for stage in 1..=4 {
            if existing && stage > 2 {
                continue;
            }
            for after in [false, true] {
                let d = tempfile::tempdir().unwrap();
                let (mut l, _) = open(d.path(), &Present);
                if existing {
                    put(&mut l, provision(1, None));
                }
                let injection = Arc::new(durability::Injection {
                    stage: AtomicU8::new(stage),
                    after,
                });
                let reg = register(
                    d.path().into(),
                    Arc::new(durability::FaultDirectory(injection.clone())),
                );
                assert!(
                    matches!(
                        l.append(
                            if existing {
                                pool(1, 1)
                            } else {
                                provision(1, None)
                            },
                            AuditActor::System,
                            SourceLayer::Layer4,
                            &Present
                        ),
                        Err(StoreError::Ambiguous(_))
                    ),
                    "stage={stage}, after={after}"
                );
                assert_eq!(injection.stage.load(Ordering::SeqCst), 0);
                assert!(matches!(
                    l.append(
                        diagnostic(),
                        AuditActor::System,
                        SourceLayer::Layer4,
                        &Present
                    ),
                    Err(StoreError::Fenced)
                ));
                let before: Vec<_> = std::fs::read_dir(d.path())
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .filter(|p| p.extension().is_some_and(|e| e == "remlib"))
                    .map(|p| (p.clone(), std::fs::read(p).unwrap()))
                    .collect();
                drop(reg);
                l.clear_ambiguity(&Present).unwrap();
                for (p, b) in before {
                    assert_eq!(std::fs::read(p).unwrap(), b);
                }
                let expected = if existing {
                    if stage == 1 && !after {
                        1
                    } else {
                        2
                    }
                } else if stage == 4 || stage == 3 && after {
                    1
                } else {
                    0
                };
                assert_eq!(
                    l.state().last_sequence,
                    expected,
                    "existing={existing}, stage={stage}, after={after}"
                );
                drop(l);
                let r = replay(d.path(), LOG, &Present).unwrap();
                assert_eq!(r.last_sequence, expected);
                assert!(r.refusal.is_none());
            }
        }
    }
}
/// Even an unsynced diagnostic write error fences subsequent authority append.
#[test]
fn library_log_diagnostic_ambiguity_fences_authority() {
    use remanence_library::durable_file::test_support::register;
    use std::sync::atomic::AtomicU8;
    for after in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let (mut l, _) = open(d.path(), &Present);
        put(&mut l, provision(1, None));
        let reg = register(
            d.path().into(),
            Arc::new(durability::FaultDirectory(Arc::new(
                durability::Injection {
                    stage: AtomicU8::new(1),
                    after,
                },
            ))),
        );
        assert!(matches!(
            l.append(
                diagnostic(),
                AuditActor::System,
                SourceLayer::Layer4,
                &Present
            ),
            Err(StoreError::Ambiguous(_))
        ));
        assert!(matches!(
            l.append(
                pool(1, 1),
                AuditActor::System,
                SourceLayer::Layer4,
                &Present
            ),
            Err(StoreError::Fenced)
        ));
        drop(reg);
        l.clear_ambiguity(&Present).unwrap();
        assert_eq!(l.state().last_sequence, if after { 2 } else { 1 });
    }
}
/// Lease ownership excludes readers and a second writer; repair preserves exact evidence.
#[test]
fn repair_archives_exact_bytes_and_refuses_interior_damage() {
    let d = tempfile::tempdir().unwrap();
    let (mut l, c) = open(d.path(), &Present);
    put(&mut l, diagnostic());
    put(&mut l, diagnostic());
    assert!(matches!(
        replay(d.path(), LOG, &Present),
        Err(StoreError::Locked(_))
    ));
    let first = l.state().segments[0].clone();
    let p = d.path().join(&first.name);
    let mut b = std::fs::read(&p).unwrap();
    let cut = 84 + 44 + 32 + u32::from_le_bytes(b[88..92].try_into().unwrap()) as u64;
    b[cut as usize + 20] ^= 1;
    std::fs::write(&p, &b).unwrap();
    // Simulate a previous uncertain archive publication that already landed.
    std::fs::write(
        d.path()
            .join(format!("{}.repair-{cut}.evidence", first.name)),
        &b,
    )
    .unwrap();
    l.repair_tail(cut, &Present).unwrap();
    assert_eq!(
        std::fs::read(
            d.path()
                .join(format!("{}.repair-{cut}.evidence", first.name))
        )
        .unwrap(),
        b
    );
    assert_eq!(l.state().last_sequence, 1);
    c.0.store(NOW + 86_400_000_000_000, Ordering::SeqCst);
    put(&mut l, diagnostic());
    let mut b = std::fs::read(&p).unwrap();
    b[104] ^= 1;
    std::fs::write(&p, b).unwrap();
    assert!(matches!(
        l.repair_tail(84, &Present),
        Err(StoreError::InteriorDamage)
    ));
}

/// Independently handwritten Python CBOR/CRC/SHA reference pins the fixed worked bytes.
#[test]
fn library_log_known_vector() {
    const HEX:&str="01002000630000002000000001000000000000003eda05ea491d6d79a200aa0050c0000000000000000000000000000001011b18e45fe7da02ca00026c666978747572652d686f737403191092047172656d616e656e63652d6669787475726505a2000001f6060307f608f609f601a4000001676669787475726502f6038099a9901a3cbf7bb90d4fc96d6c9a0162617edea6cb4ec7a25dfcbcfdf53cb594e03ba6316768bb027c231966e661e2e3";
    let expected: Vec<u8> = HEX
        .as_bytes()
        .chunks_exact(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect();
    assert_eq!(remanence_crc::crc64_xz(b"123456789"), 0x995dc9bbdf1939fa);
    let (actual, hash) = codec::encode(1, [0; 32], &record(diagnostic(), 1)).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(&actual[actual.len() - 40..actual.len() - 8], hash);
}
/// Read-only replay of an empty directory creates no lock or other file.
#[test]
fn replay_is_read_only_even_without_lock_file() {
    let d = tempfile::tempdir().unwrap();
    let r = replay(d.path(), LOG, &Present).unwrap();
    assert_eq!(r.last_sequence, 0);
    assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
}
/// Retrying native resolution reaches a fixed point without replaying applied decisions.
#[test]
fn pending_resolves_to_fixed_point() {
    let mut f = initialized();
    for b in [progress(1), progress(2), pool(1, 1)] {
        let n = f.received_through + 1;
        let r = record(b, n);
        f.receive(
            n,
            r.clone(),
            receipt(&r, n),
            &View::default(),
            &History::default(),
        )
        .unwrap();
    }
    assert_eq!(f.pending.len(), 3);
    f.resolve(&Present, &History::default()).unwrap();
    assert!(f.pending.is_empty());
    assert_eq!(f.applied_through, 5);
    let generation = f.tapes[&tape(1).uuid].assignment_generation;
    f.resolve(&Present, &History::default()).unwrap();
    assert_eq!(f.tapes[&tape(1).uuid].assignment_generation, generation);
}
/// Fence evidence never becomes a dependency, even when the referenced native frame is absent.
#[test]
fn fence_raised_never_waits() {
    let mut f = initialized();
    feed(&mut f, fence(1), &View::default()).unwrap();
    assert!(f.pending.is_empty());
    assert_eq!(f.open_fences(), 1);
    assert_eq!(f.unresolved_fence_evidence.len(), 1);
}
/// Inventory reconciliation clears only named uncertainty in its own changer.
#[test]
fn inventory_unknown_is_library_scoped() {
    let mut f = initialized();
    feed(&mut f, intent(1), &Present).unwrap();
    let b = Body::InventoryReconciled(InventoryReconciled {
        library_serial: "LIBA0001".into(),
        observation_generation: 1,
        resolves: Bounded(vec![DecisionRef {
            sequence: 3,
            record_uuid: Ids.next(3),
        }]),
        complete: false,
        elements: Bounded(vec![]),
    });
    assert_eq!(
        feed(&mut f, b, &Present).unwrap_err(),
        FoldError::InventoryScope
    );
    assert!(!f.inventory_unknown(&fold::Scope::Library("LIBB0002".into())));
    let b = Body::InventoryReconciled(InventoryReconciled {
        library_serial: "LIBA0001".into(),
        observation_generation: 1,
        resolves: Bounded(vec![DecisionRef {
            sequence: 3,
            record_uuid: Ids.next(3),
        }]),
        complete: true,
        elements: Bounded(vec![]),
    });
    feed(&mut f, b, &Present).unwrap();
    assert!(!f.inventory_unknown(&fold::Scope::Library("LIBA0001".into())));
}
/// Scalar bounds reject before reading hostile lengths; diagnostic keys and negatives are typed.
#[test]
fn bounded_schema_refuses_hostile_scalars() {
    use crate::log_frame::cbor::Wire;
    assert!(Text::get(&mut &[0x79, 0x10, 0x01][..]).is_err());
    assert!(LabelText::get(&mut &[0x78, 33][..]).is_err());
    assert!(Blob::<252>::get(&mut &[0x58, 253][..]).is_err());
    let mut sink = Vec::new();
    assert!(Negative(0).put(&mut sink).is_err());
    let d = Body::Diagnostic(Diagnostic {
        event: DiagnosticEvent::HardwareWarning,
        subject_kind: "drive".into(),
        subject_id: None,
        detail: Bounded(vec![
            DiagnosticEntry("z".into(), DiagnosticValue::Null(Unit)),
            DiagnosticEntry("a".into(), DiagnosticValue::Null(Unit)),
        ]),
    });
    assert_eq!(
        feed(&mut LibraryFold::default(), d, &Present).unwrap_err(),
        FoldError::InvalidSchema
    );
}
/// Concurrent cleaning and envelope/body operation identity contradictions cannot be applied.
#[test]
fn cleaning_and_attribution_rules() {
    let mut f = initialized();
    let b = Body::CleaningRecorded(CleaningRecorded {
        run_id: "one".into(),
        previous: None,
        drive_uuid: Some([1; 16]),
        cartridge: Some(tape(1)),
        action: CleaningAction::Begin(Begin {
            trigger: CleanTrigger::Manual,
        }),
    });
    feed(&mut f, b.clone(), &Present).unwrap();
    let mut second = b;
    if let Body::CleaningRecorded(c) = &mut second {
        c.run_id = "two".into();
    }
    assert_eq!(
        feed(&mut f, second, &Present).unwrap_err(),
        FoldError::CleaningBusy
    );
    let mut r = record(progress(1), 4);
    r.envelope.operation_id = Some([0; 16]);
    assert_eq!(
        f.validate(&r, &Present, &History::default()).unwrap_err(),
        FoldError::OperationIdentity
    );
    assert_eq!(
        f.apply(
            99,
            r.clone(),
            receipt(&r, 99),
            &Present,
            &History::default()
        )
        .unwrap_err(),
        FoldError::Sequence
    );
}

/// Exercises the production corpus helper for `every_tag_round_trips_canonically`.
#[test]
fn every_tag_round_trips_canonically() {
    super::reference_fixtures::every_tag_round_trips_canonically();
}

/// Exercises the production corpus helper for `append_and_replay_share_validate`.
#[test]
fn append_and_replay_share_validate() {
    super::reference_fixtures::append_and_replay_share_validate();
}

/// Exercises the production corpus helper for `tape_reference_ahead_of_tape_log_waits`.
#[test]
fn tape_reference_ahead_of_tape_log_waits() {
    super::reference_fixtures::tape_reference_ahead_of_tape_log_waits();
}

/// Identical live retries return the original receipt and never extend a segment.
#[test]
fn append_duplicate_writes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let (mut l, _) = open(d.path(), &Present);
    put(&mut l, provision(1, None));
    let original = put(&mut l, pool(1, 1));
    let tail = l.state().segments[0].through;
    let retried = put(&mut l, pool(1, 1));
    assert_eq!(original, retried);
    assert_eq!(tail, l.state().segments[0].through);
    assert_eq!(l.state().last_sequence, 2);
    assert!(matches!(
        l.append(
            pool(1, 99),
            AuditActor::System,
            SourceLayer::Layer4,
            &Present
        ),
        Err(StoreError::Fold(FoldError::GenerationMismatch))
    ));
    assert_eq!(tail, l.state().segments[0].through);
}
/// A native mismatch must not hide a writer-impossible local generation.
#[test]
fn library_contradiction_precedes_native_contradiction() {
    let mut f = initialized();
    let b = Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
        at: at(1, 1),
        operation: [5; 16],
        actor_fingerprint: "operator".into(),
        idempotency_key: [5; 16],
        request_fingerprint: [6; 32],
        assignment_generation: 90,
    });
    let view = View(BTreeMap::from([(
        (tape(1).uuid, 1),
        TapeLookup::Contradiction,
    )]));
    let n = f.received_through + 1;
    let r = record(b, n);
    assert_eq!(
        f.receive(n, r.clone(), receipt(&r, n), &view, &History::default()),
        Err(FoldError::GenerationMismatch)
    );
}

/// Endpoint generations, last-readiness linkage and terminality are independent constraints.
#[test]
fn readiness_chain_generation_and_terminal_rules() {
    let mut f = initialized();
    let mut v = ReadinessObserved {
        operation: [90; 16],
        endpoint: endpoint("LIBA0001"),
        tape: None,
        barcode: None,
        phase: ReadinessPhase::Planned,
        state: ReadinessState::Planned,
        previous: None,
        started_unix_ns: 0,
        limit_ms: 1,
        evidence: None,
        load_intent: None,
        ready_generation: 1,
        result: ReadinessResult::Pending,
    };
    let previous = record(Body::ReadinessObserved(v.clone()), 3).reference(3);
    feed(&mut f, Body::ReadinessObserved(v.clone()), &Present).unwrap();
    v.previous = Some(previous);
    v.ready_generation = 1;
    assert_eq!(
        feed(&mut f, Body::ReadinessObserved(v.clone()), &Present).unwrap_err(),
        FoldError::GenerationMismatch
    );
    v.ready_generation = 2;
    v.result = ReadinessResult::Ready;
    feed(&mut f, Body::ReadinessObserved(v.clone()), &Present).unwrap();
    let history = History(BTreeMap::from([(
        4,
        record(Body::ReadinessObserved(v.clone()), 4),
    )]));
    v.previous = Some(record(diagnostic(), 4).reference(4));
    v.ready_generation = 3;
    assert_eq!(
        f.validate(&record(Body::ReadinessObserved(v), 5), &Present, &history)
            .unwrap_err(),
        FoldError::OperationState
    );
}
/// Idempotency scope binds the operation and fingerprint even after that operation advances.
#[test]
fn operation_idempotency_is_not_envelope_identity() {
    let mut f = initialized();
    let mut v = OperationRecorded {
        operation: [90; 16],
        kind: OperationKind::TapeVerification,
        previous: None,
        event: OperationEvent::Accepted(Accepted {
            actor_fingerprint: "actor".into(),
            idempotency_key: Some([91; 16]),
            request_fingerprint: [92; 32],
            subject: None,
        }),
    };
    feed(&mut f, Body::OperationRecorded(v.clone()), &Present).unwrap();
    feed(
        &mut f,
        Body::OperationRecorded(OperationRecorded {
            operation: v.operation,
            kind: v.kind,
            previous: Some(decision(3)),
            event: OperationEvent::Finished(Response {
                response_fingerprint: None,
            }),
        }),
        &Present,
    )
    .unwrap();
    assert_eq!(f.retained_records(), 2);
    assert_eq!(f.idempotency_bytes(), 80);
    v.operation = [93; 16];
    assert_eq!(
        feed(&mut f, Body::OperationRecorded(v), &Present).unwrap_err(),
        FoldError::ConflictingDuplicate
    );
}
/// Direct callers cannot skip sequences; retirement detail is presentation only.
#[test]
fn sequence_and_retirement_detail_rules() {
    let mut f = initialized();
    let r = record(diagnostic(), 9);
    assert_eq!(
        f.apply(9, r.clone(), receipt(&r, 9), &Present, &History::default())
            .unwrap_err(),
        FoldError::Sequence
    );
    let mut other = f.clone();
    let mut retire = Retire {
        tape: tape(1),
        reason: RetireReason::OperatorRequested,
        observed_tape_sequence: Some(u64::MAX),
        assignment_generation: 1,
        detail: None,
    };
    feed(&mut f, Body::Retire(retire.clone()), &Present).unwrap();
    retire.detail = Some("  verbatim\noperator detail  ".into());
    let r = record(Body::Retire(retire.clone()), 3);
    let (bytes, _) = codec::encode(3, [0; 32], &r).unwrap();
    let mut decoder = codec::Decoder::at(bytes.as_slice(), 3, 0);
    assert_eq!(decoder.next_frame().unwrap().unwrap().record, r);
    feed(&mut other, Body::Retire(retire), &Present).unwrap();
    assert_eq!(f.tapes, other.tapes);
}

/// Cartridge selection after Begin is valid but must preserve active-cartridge exclusion.
#[test]
fn cleaning_late_cartridge_selection_checks_exclusion() {
    let mut f = initialized();
    let mut first = CleaningRecorded {
        run_id: "first".into(),
        previous: None,
        drive_uuid: Some([31; 16]),
        cartridge: None,
        action: CleaningAction::Begin(Begin {
            trigger: CleanTrigger::Manual,
        }),
    };
    feed(&mut f, Body::CleaningRecorded(first.clone()), &Present).unwrap();
    first.previous = Some(record(diagnostic(), 3).reference(3));
    first.cartridge = Some(tape(1));
    first.action = CleaningAction::Advance(Advance {
        phase: CleanPhase::Selecting,
    });
    feed(&mut f, Body::CleaningRecorded(first), &Present).unwrap();
    let mut other = CleaningRecorded {
        run_id: "second".into(),
        previous: None,
        drive_uuid: Some([32; 16]),
        cartridge: None,
        action: CleaningAction::Begin(Begin {
            trigger: CleanTrigger::Manual,
        }),
    };
    feed(&mut f, Body::CleaningRecorded(other.clone()), &Present).unwrap();
    other.previous = Some(record(diagnostic(), 5).reference(5));
    other.cartridge = Some(tape(1));
    other.action = CleaningAction::Advance(Advance {
        phase: CleanPhase::Selecting,
    });
    assert_eq!(
        feed(&mut f, Body::CleaningRecorded(other), &Present).unwrap_err(),
        FoldError::CleaningBusy
    );
}

/// Evidence-only references still obey scalar bounds, and parent operations bind attribution.
#[test]
fn fence_evidence_and_motion_attribution_are_typed() {
    let f = initialized();
    let mut b = fence(1);
    if let Body::FenceRaised(v) = &mut b {
        v.at = Some(at(1, 0));
    }
    assert_eq!(
        f.validate(&record(b, 3), &Present, &History::default())
            .unwrap_err(),
        FoldError::InvalidSchema
    );
    let mut r = record(intent(1), 3);
    r.envelope.operation_id = Some([250; 16]);
    assert_eq!(
        f.validate(&r, &Present, &History::default()).unwrap_err(),
        FoldError::OperationIdentity
    );
    for date in [
        *b"2025-02-29",
        *b"2026-00-01",
        *b"0000-01-01",
        *b"2026-04-31",
    ] {
        assert!(codec::Header {
            log_uuid: LOG,
            first_sequence: 1,
            previous_segment_hash: [0; 32],
            date
        }
        .encode()
        .is_err());
    }
    assert!(codec::Header {
        log_uuid: LOG,
        first_sequence: 1,
        previous_segment_hash: [0; 32],
        date: *b"2024-02-29"
    }
    .encode()
    .is_ok());
}

/// Register/Expire describe a cartridge and cannot erase an active run with the same text id.
#[test]
fn cleaning_cartridge_records_do_not_replace_run_state() {
    let mut f = initialized();
    let begin = CleaningRecorded {
        run_id: "run".into(),
        previous: None,
        drive_uuid: Some([31; 16]),
        cartridge: Some(tape(1)),
        action: CleaningAction::Begin(Begin {
            trigger: CleanTrigger::Manual,
        }),
    };
    feed(&mut f, Body::CleaningRecorded(begin.clone()), &Present).unwrap();
    let registration = CleaningRecorded {
        run_id: "run".into(),
        previous: None,
        drive_uuid: None,
        cartridge: Some(tape(1)),
        action: CleaningAction::Register(Register {
            barcode: "CLN".into(),
            home_slot: 1,
        }),
    };
    feed(&mut f, Body::CleaningRecorded(registration), &Present).unwrap();
    let mut other = begin;
    other.run_id = "other".into();
    other.drive_uuid = Some([32; 16]);
    assert_eq!(
        feed(&mut f, Body::CleaningRecorded(other), &Present).unwrap_err(),
        FoldError::CleaningBusy
    );
}

/// Canonical configuration for current-versus-superseded assignment tests.
pub(super) fn configuration(generation: u64) -> Body {
    let definitions = Definitions {
        pools: Bounded(vec![]),
        rules: Bounded(vec![]),
        cleaning_prefixes: Bounded(vec![]),
    };
    Body::ConfigurationAccepted(ConfigurationAccepted {
        generation,
        config_digest: codec::configuration_digest(&definitions).unwrap(),
        definitions,
    })
}
/// Readiness can cite a LOAD after its outcome has removed the open intent.
pub(super) fn readiness_for(reference: DecisionRef) -> Body {
    Body::ReadinessObserved(ReadinessObserved {
        operation: [90; 16],
        endpoint: endpoint("LIBA0001"),
        tape: None,
        barcode: None,
        phase: ReadinessPhase::Planned,
        state: ReadinessState::Planned,
        previous: None,
        started_unix_ns: 0,
        limit_ms: 1,
        evidence: None,
        load_intent: Some(reference),
        ready_generation: 1,
        result: ReadinessResult::Pending,
    })
}
/// Reference identities are deterministic in both memory and disk fixtures.
pub(super) fn decision(n: u64) -> DecisionRef {
    DecisionRef {
        sequence: n,
        record_uuid: Ids.next(n),
    }
}
/// Recovery's prior may have been replaced in the retained operation slot.
pub(super) fn recovery(prior: Option<DecisionRef>) -> Body {
    Body::RecoveryRecorded(RecoveryRecorded {
        at: at(1, 1),
        operation: [91; 16],
        prior,
        action: RecoveryAction::AppendAuthorityRecovered(Recovered {
            source_digest: [1; 32],
        }),
    })
}
/// Historical cases share a two-tape prefix with the rejected corpus builder.
pub(super) fn historical_cases() -> Vec<(&'static str, Vec<Body>, Body, Option<FoldError>)> {
    let outcome = Body::MotionOutcome(MotionOutcome {
        intent: decision(3),
        result: MotionResult::Failed,
        evidence: evidence(),
        confirmation: None,
    });
    let mut moved = intent(1);
    if let Body::MotionIntent(v) = &mut moved {
        v.motion = Motion::Move(Move {
            library_serial: "LIBA0001".into(),
            source: 1,
            destination: 2,
        });
    }
    let mut rule = pool(1, 1);
    if let Body::AssignPool(v) = &mut rule {
        v.source = AssignmentSource::Rule;
        v.configuration = Some(decision(3));
    }
    let alarm = Body::AlarmChanged(AlarmChanged {
        condition_key: "history-alarm".into(),
        change: GenerationChange {
            expected: 0,
            next: 1,
        },
        action: AlarmAction::Raise(AlarmRaise {
            kind: AlarmKind::SnapshotPersistFailing,
            severity: Severity::Warning,
            reason: "history".into(),
        }),
        cause: Some(decision(3)),
    });
    vec![
        (
            "old-fence-after-later-release",
            vec![fence(1), release(3), fence(1), release(5)],
            release(3),
            Some(FoldError::FenceAlreadyReleased),
        ),
        (
            "release-historical-wrong-tag",
            vec![pool(1, 1), pool(1, 2)],
            release(3),
            Some(FoldError::WrongCounterpart),
        ),
        (
            "resolved-load-readiness",
            vec![intent(1), outcome.clone()],
            readiness_for(decision(3)),
            None,
        ),
        (
            "resolved-move-readiness",
            vec![moved, outcome],
            readiness_for(decision(3)),
            Some(FoldError::WrongCounterpart),
        ),
        (
            "historical-wrong-tag",
            vec![diagnostic()],
            readiness_for(decision(3)),
            Some(FoldError::WrongCounterpart),
        ),
        (
            "stale-configuration",
            vec![configuration(1), configuration(2)],
            rule,
            Some(FoldError::StaleConfiguration),
        ),
        (
            "historical-alarm-cause",
            vec![pool(1, 1), pool(1, 2)],
            alarm,
            None,
        ),
        (
            "historical-recovery-prior",
            vec![recovery(None), recovery(Some(decision(3)))],
            recovery(Some(decision(3))),
            None,
        ),
        (
            "historical-recovery-wrong-tag",
            vec![pool(1, 1), pool(1, 2)],
            recovery(Some(decision(3))),
            Some(FoldError::WrongCounterpart),
        ),
        (
            "historical-uuid-mismatch",
            vec![diagnostic()],
            readiness_for(DecisionRef {
                sequence: 3,
                record_uuid: [99; 16],
            }),
            Some(FoldError::WrongCounterpart),
        ),
    ]
}
/// In-memory history and the segment store agree on replaced references and exact errors.
#[test]
fn historical_references_use_memory_and_store_views() {
    for (name, prefix, candidate, expected) in historical_cases() {
        let mut f = LibraryFold::default();
        let mut history = History::default();
        let d = tempfile::tempdir().unwrap();
        let (mut log, clock) = open(d.path(), &Present);
        for b in [provision(1, Some("A")), provision(2, Some("B"))]
            .into_iter()
            .chain(prefix)
        {
            let n = f.received_through + 1;
            let r = record(b.clone(), n);
            f.apply(n, r.clone(), receipt(&r, n), &Present, &history)
                .unwrap();
            history.0.insert(n, r);
            // Multiple segments exercise the header binary search, including old segments.
            clock
                .0
                .store(NOW + n * 86_400_000_000_000, Ordering::SeqCst);
            put(&mut log, b);
        }
        let n = f.received_through + 1;
        let result = f.validate(&record(candidate.clone(), n), &Present, &history);
        let on_disk = log.append(candidate, AuditActor::System, SourceLayer::Layer4, &Present);
        if let Some(expected) = expected {
            assert_eq!(result.unwrap_err(), expected, "{name}");
            assert!(
                matches!(on_disk, Err(StoreError::Fold(e)) if e == expected),
                "{name}"
            );
        } else {
            result.unwrap();
            on_disk.unwrap();
        }
        assert!(log.lookup(decision(3)).unwrap().is_some());
        assert!(log.lookup_fence(3).unwrap().is_some());
        assert!(log.lookup_fence(0).unwrap().is_none());
        assert!(log
            .lookup_fence(log.state().last_sequence + 1)
            .unwrap()
            .is_none());
        assert!(log
            .lookup(DecisionRef {
                sequence: 3,
                record_uuid: [99; 16]
            })
            .unwrap()
            .is_none());
        assert!(log
            .lookup(decision(log.state().last_sequence + 1))
            .unwrap()
            .is_none());
        drop(log);
        let r = replay(d.path(), LOG, &Present).unwrap();
        assert!(r.refusal.is_none(), "{name}: {:?}", r.refusal);
    }
}
/// Rule references distinguish superseded configurations from unrelated records.
#[test]
fn rule_configuration_classifies_historical_kind() {
    let mut f = initialized();
    feed(&mut f, configuration(1), &Present).unwrap();
    feed(&mut f, configuration(2), &Present).unwrap();
    let mut b = pool(1, 1);
    if let Body::AssignPool(v) = &mut b {
        v.source = AssignmentSource::Rule;
        v.configuration = Some(decision(3));
    }
    let mut history = History::default();
    history.0.insert(3, record(configuration(1), 3));
    assert_eq!(
        f.validate(&record(b.clone(), 5), &Present, &history)
            .unwrap_err(),
        FoldError::StaleConfiguration
    );
    history.0.insert(3, record(diagnostic(), 3));
    assert_eq!(
        f.validate(&record(b, 5), &Present, &history).unwrap_err(),
        FoldError::WrongCounterpart
    );
}
/// A history read failure fails closed instead of becoming a missing counterpart.
#[test]
fn historical_io_failure_is_typed() {
    struct Failed;
    impl LibraryHistory for Failed {
        fn lookup_fence(&self, _: u64) -> Result<Option<Record>, FoldError> {
            Err(FoldError::HistoryUnavailable)
        }
        fn lookup(&self, _: DecisionRef) -> Result<Option<Record>, FoldError> {
            Err(FoldError::HistoryUnavailable)
        }
    }
    let mut f = initialized();
    feed(&mut f, diagnostic(), &Present).unwrap();
    assert_eq!(
        f.validate(&record(release(3), 4), &Present, &Failed)
            .unwrap_err(),
        FoldError::HistoryUnavailable
    );
    assert_eq!(
        f.validate(&record(readiness_for(decision(3)), 4), &Present, &Failed)
            .unwrap_err(),
        FoldError::HistoryUnavailable
    );
}
/// The 4097th pending record refuses without advancing the prefix or losing the first 4096.
#[test]
fn pending_record_cap_replay_and_append() {
    let d = tempfile::tempdir().unwrap();
    super::reference_fixtures::pending_limit_case(d.path());
    let r = replay(d.path(), LOG, &View::default()).unwrap();
    assert_eq!(r.fold.pending.len(), MAX_PENDING_RECORDS);
    assert_eq!(r.fold.received_through, MAX_PENDING_RECORDS as u64 + 1);
    assert_eq!(r.fold.applied_through, 1);
    assert!(matches!(
        r.refusal.unwrap().fault,
        RefusalKind::Fold(FoldError::PendingLimitExceeded { records: 4097, .. })
    ));
    // A valid prefix at the cap opens normally; an append to that scope writes nothing.
    let d = tempfile::tempdir().unwrap();
    super::reference_fixtures::pending_limit_prefix(d.path(), MAX_PENDING_RECORDS);
    let (mut log, _) = open(d.path(), &View::default());
    let before = log.state().segments[0].through;
    let err = log
        .append(
            progress(4097),
            AuditActor::System,
            SourceLayer::Layer4,
            &View::default(),
        )
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::Fold(FoldError::PendingLimitExceeded { records: 4097, .. })
    ));
    assert_eq!(log.state().segments[0].through, before);
    assert_eq!(
        std::fs::metadata(d.path().join(&log.state().segments[0].name))
            .unwrap()
            .len(),
        before
    );
    assert_eq!(log.state().fold.pending.len(), MAX_PENDING_RECORDS);
    put(&mut log, progress(4097));
    assert!(log.state().fold.pending.is_empty());
    assert_eq!(log.state().fold.pending_bytes(), 0);
}
/// Byte capacity is enforced independently of record count, including prospective appends.
#[test]
fn pending_byte_cap_is_exact() {
    let mut f = initialized();
    let r = record(progress(1), 3);
    f.receive(
        3,
        r.clone(),
        receipt(&r, 3),
        &View::default(),
        &History::default(),
    )
    .unwrap();
    let bytes = vec![0; 4 * 1024 * 1024];
    let b = Body::WrapMapCached(WrapMapCached {
        tape: tape(1),
        calibration_generation: 1,
        action: WrapAction::Cache(Cache {
            descriptors: Blob(bytes),
            mapped_extent_lba: 1,
            write_epoch: 1,
            digest: [0; 32],
        }),
    });
    loop {
        let n = f.received_through + 1;
        let r = record(b.clone(), n);
        let before = f.pending_bytes();
        let result = f.receive(
            n,
            r.clone(),
            receipt(&r, n),
            &View::default(),
            &History::default(),
        );
        if let Err(e) = result {
            assert!(
                matches!(e, FoldError::PendingLimitExceeded { records, bytes, .. } if records < MAX_PENDING_RECORDS && bytes > MAX_PENDING_BYTES)
            );
            assert_eq!(f.pending_bytes(), before);
            assert_eq!(
                f.validate(&r, &View::default(), &History::default())
                    .unwrap_err(),
                e
            );
            // Fit the last opaque body to the exact byte boundary. Its CBOR
            // length header remains four bytes, so reducing the blob is exact.
            let mut encoded = Vec::new();
            wire::encode(&r, &mut encoded).unwrap();
            let excess = before + encoded.len() - MAX_PENDING_BYTES;
            let mut fitted = r.clone();
            let Body::WrapMapCached(v) = &mut fitted.body else {
                unreachable!()
            };
            let WrapAction::Cache(c) = &mut v.action else {
                unreachable!()
            };
            c.descriptors.0.resize(c.descriptors.0.len() - excess, 0);
            f.receive(
                n,
                fitted.clone(),
                receipt(&fitted, n),
                &View::default(),
                &History::default(),
            )
            .unwrap();
            assert_eq!(f.pending_bytes(), MAX_PENDING_BYTES);
            let next = record(progress(2), n + 1);
            let err = f
                .validate(&next, &View::default(), &History::default())
                .unwrap_err();
            assert!(
                matches!(err, FoldError::PendingLimitExceeded { bytes, .. } if bytes > MAX_PENDING_BYTES)
            );
            assert_eq!(
                f.receive(
                    n + 1,
                    next.clone(),
                    receipt(&next, n + 1),
                    &View::default(),
                    &History::default()
                )
                .unwrap_err(),
                err
            );
            assert_eq!(f.pending_bytes(), MAX_PENDING_BYTES);
            break;
        }
        assert!(f.pending_bytes() <= MAX_PENDING_BYTES);
    }
}

/// Even an otherwise identical rule retry must use the current configuration.
#[test]
fn identical_rule_retry_under_superseded_configuration_is_stale() {
    let mut f = initialized();
    feed(&mut f, configuration(1), &Present).unwrap();
    let mut assignment = pool(1, 1);
    if let Body::AssignPool(v) = &mut assignment {
        v.source = AssignmentSource::Rule;
        v.configuration = Some(decision(3));
    }
    feed(&mut f, assignment.clone(), &Present).unwrap();
    feed(&mut f, configuration(2), &Present).unwrap();
    assert_eq!(
        f.validate(
            &record(assignment, 6),
            &Present,
            &History(BTreeMap::from([(3, record(configuration(1), 3))]))
        )
        .unwrap_err(),
        FoldError::StaleConfiguration
    );
}

/// A fence wire reference carries only the raise sequence.
pub(super) fn release(sequence: u64) -> Body {
    Body::FenceReleased(FenceReleased {
        fence: sequence,
        evidence: "proof".into(),
        forced: false,
    })
}
/// A known drive for the latest-snapshot contract cases.
fn health_drive(n: u8) -> Body {
    Body::DriveObserved(DriveObserved {
        drive_uuid: [n; 16],
        serial: None,
        identity_source: IdentitySource::Standalone,
        endpoint: None,
        managed: Managed::Foreign,
        inventory_generation: 1,
        vendor: None,
        product: None,
        firmware_rev: None,
    })
}
/// Cumulative snapshot values; the session key is distinct from the envelope UUID.
fn health(n: u8, session: Option<Id>, count: u64) -> Body {
    Body::DriveHealthObserved(DriveHealthObserved {
        drive_uuid: [n; 16],
        tape: None,
        session_id: session,
        operation: None,
        trigger: HealthTrigger::SessionClose,
        tape_alert_flags: 0,
        counters: Counters {
            write_corrected: Some(count),
            write_uncorrected: None,
            read_corrected: None,
            read_uncorrected: None,
        },
        raw_pages: None,
    })
}
/// Shared append, replay and corpus cases for the per-drive latest-snapshot guard.
pub(super) fn health_cases() -> Vec<(&'static str, Vec<Body>, Body, Option<FoldError>)> {
    let snapshot = health(30, Some([40; 16]), 7);
    vec![
        (
            "health-identical-latest",
            vec![health_drive(30), snapshot.clone()],
            snapshot.clone(),
            None,
        ),
        (
            "health-conflicting-latest",
            vec![health_drive(30), snapshot.clone()],
            health(30, Some([40; 16]), 8),
            Some(FoldError::ConflictingDuplicate),
        ),
        (
            "health-replaced-key",
            vec![
                health_drive(30),
                snapshot.clone(),
                health(30, Some([41; 16]), 8),
            ],
            snapshot.clone(),
            None,
        ),
        (
            "health-other-drive-key",
            vec![health_drive(30), health_drive(31), snapshot],
            health(31, Some([40; 16]), 8),
            None,
        ),
        (
            "health-without-session",
            vec![health_drive(30), health(30, None, 7)],
            health(30, None, 8),
            None,
        ),
    ]
}
/// Only a drive's latest keyed snapshot participates in fold deduplication.
#[test]
fn health_latest_snapshot_guard_and_append_noop() {
    for (name, prefix, candidate, expected) in health_cases() {
        let mut f = initialized();
        let d = tempfile::tempdir().unwrap();
        let (mut log, _) = open(d.path(), &Present);
        put(&mut log, provision(1, Some("A")));
        put(&mut log, provision(2, Some("B")));
        for b in prefix {
            feed(&mut f, b.clone(), &Present).unwrap();
            put(&mut log, b);
        }
        let before = log.state().last_sequence;
        let bytes = log.state().segments.last().unwrap().through;
        let result = feed(&mut f, candidate.clone(), &Present);
        let stored = log.append(candidate, AuditActor::System, SourceLayer::Layer4, &Present);
        if let Some(error) = expected {
            assert_eq!(result.unwrap_err(), error, "{name}");
            assert!(
                matches!(stored, Err(StoreError::Fold(e)) if e == error),
                "{name}"
            );
            assert_eq!(log.state().last_sequence, before);
            assert_eq!(log.state().segments.last().unwrap().through, bytes);
        } else if name == "health-identical-latest" {
            assert!(matches!(result.unwrap(), fold::Validated::Duplicate(_)));
            assert_eq!(stored.unwrap().sequence, before);
            assert_eq!(log.state().last_sequence, before);
            assert_eq!(log.state().segments.last().unwrap().through, bytes);
            assert_eq!(f.duplicates, 1);
        } else {
            assert!(matches!(result.unwrap(), fold::Validated::Apply));
            assert_eq!(stored.unwrap().sequence, before + 1);
            assert_eq!(f.duplicates, 0);
        }
        drop(log);
        assert!(
            replay(d.path(), LOG, &Present).unwrap().refusal.is_none(),
            "{name}"
        );
    }
}
/// Absence in an injected history is distinct from a historical wrong tag.
#[test]
fn release_without_fence_uses_absent_history() {
    let mut f = LibraryFold::default();
    feed(&mut f, diagnostic(), &Present).unwrap();
    assert_eq!(
        feed(&mut f, release(1), &Present).unwrap_err(),
        FoldError::ReleaseWithoutFence
    );
}

/// A power cut after rotation preserves the entire chain of each synced receipt.
#[test]
fn rotation_syncs_previous_diagnostics_before_publication() {
    use remanence_library::durable_file::test_support::register;
    for authority in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let cache = durability::PageCache::default();
        let reg = register(d.path().into(), Arc::new(cache.clone()));
        let (mut l, clock) = open(d.path(), &Present);
        let mut receipts = vec![put(&mut l, provision(1, None))];
        for _ in 0..3 {
            let r = put(&mut l, diagnostic());
            assert!(!r.synced);
            receipts.push(r);
        }
        clock.0.fetch_add(86_400_000_000_000, Ordering::SeqCst);
        receipts.push(put(
            &mut l,
            if authority { pool(1, 1) } else { diagnostic() },
        ));
        assert!(receipts.last().unwrap().synced);
        drop(l);
        cache.crash();
        drop(reg);
        let r = replay(d.path(), LOG, &Present).unwrap();
        assert!(r.refusal.is_none(), "{:?}", r.refusal);
        for receipt in receipts.into_iter().filter(|r| r.synced) {
            assert!(r.last_sequence >= receipt.sequence);
        }
        assert_eq!(r.last_sequence, 5);
    }
}

/// Failure of the old-file barrier must fence before publishing the successor.
#[test]
fn rotation_old_segment_sync_failure_is_ambiguous() {
    use remanence_library::durable_file::test_support::register;
    for after in [false, true] {
        let d = tempfile::tempdir().unwrap();
        let (mut l, clock) = open(d.path(), &Present);
        put(&mut l, provision(1, None));
        put(&mut l, diagnostic());
        clock.0.fetch_add(86_400_000_000_000, Ordering::SeqCst);
        let reg = register(
            d.path().into(),
            Arc::new(durability::FaultDirectory(Arc::new(
                durability::Injection {
                    stage: std::sync::atomic::AtomicU8::new(2),
                    after,
                },
            ))),
        );
        assert!(matches!(
            l.append(
                pool(1, 1),
                AuditActor::System,
                SourceLayer::Layer4,
                &Present
            ),
            Err(StoreError::Ambiguous(_))
        ));
        assert!(d.path().join("library-log.ambiguous").exists());
        assert_eq!(l.state().last_sequence, 2);
        // Old-file sync occurs before even constructing the next temporary.
        assert!(!std::fs::read_dir(d.path()).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".segment-")));
        drop(reg);
        l.clear_ambiguity(&Present).unwrap();
        assert_eq!(l.state().last_sequence, 2);
    }
}

/// There must be a single implementation of canonical struct-map key ordering.
#[test]
fn fields_macro_delegates_shared_map_codec() {
    let source = include_str!("wire.rs");
    let fields = source
        .split("macro_rules! fields {")
        .nth(1)
        .unwrap()
        .split("macro_rules! enumeration")
        .next()
        .unwrap();
    assert!(fields.contains("crate::log_frame::cbor::fields!($ty; $($field),+)"));
    assert!(!fields.contains("impl Wire for"));
}

/// Every simple rejected vector declares the rule it is intended to exercise.
#[test]
fn semantic_vectors_pin_expected_errors() {
    let cases: Vec<(&str, FoldError, Body)> = super::reference_fixtures::semantic_cases();
    assert_eq!(cases.len(), 10);
    let (_, expected, body) = cases
        .iter()
        .find(|(name, _, _)| *name == "release-wrong-counterpart")
        .unwrap();
    assert_eq!(*expected, FoldError::WrongCounterpart);
    assert!(matches!(body, Body::FenceReleased(_)));
}

/// Common LTO capabilities for library history fixtures.
pub(super) fn medium_spec() -> MediumSpec {
    MediumSpec {
        family: MediumFamily::Lto(LtoGen::Lto9),
        usable_logical_bytes: 18_000_000_000_000,
        record_size: 1048576,
        rerecord: false,
        lifecycle_ops: false,
        source: GenerationSource::Measured,
    }
}

/// Revise the whole spec under the existing media generation check.
pub(super) fn medium_change(medium: MediumSpec, expected: u64) -> Body {
    Body::MediumSpecSet(MediumSpecSet {
        tape: tape(1),
        medium,
        change: GenerationChange {
            expected,
            next: expected + 1,
        },
    })
}
/// Each reserved vocabulary item has an otherwise ordinary fence shape.
pub(super) fn reserved_fence(model: bool) -> Body {
    Body::FenceRaised(FenceRaised {
        kind: if model {
            FenceKind::DriveFault
        } else {
            FenceKind::Write
        },
        scope: if model {
            FenceScope::Model {
                model: "HC560".into(),
                firmware: "A1".into(),
            }
        } else {
            FenceScope::Tape(tape(1))
        },
        cause: if model {
            FenceCause::TransferError
        } else {
            FenceCause::ContainerQuarantine
        },
        evidence: None,
        at: None,
        operation: None,
    })
}
/// Disk provisioning retains capabilities through live append and decoded replay.
#[test]
fn disk_reprovision_retains_medium_spec() {
    let mut f = LibraryFold::default();
    let mut b = provision(1, None);
    let Body::Reprovision(v) = &mut b else {
        unreachable!()
    };
    v.medium = MediumSpec {
        family: MediumFamily::Disk,
        rerecord: true,
        lifecycle_ops: true,
        ..medium_spec()
    };
    let expected = v.medium.clone();
    feed(&mut f, b, &Present).unwrap();
    assert_eq!(f.tapes[&tape(1).uuid].medium, expected);
    assert_eq!(f.tapes[&tape(1).uuid].media_revision, 1);
}
/// Family and capacity rules apply to the latest committed spec, never just provisioning.
#[test]
fn medium_spec_revision_rules() {
    let mut f = LibraryFold::default();
    feed(&mut f, provision(1, None), &Present).unwrap();
    for family in [MediumFamily::Disk, MediumFamily::Lto(LtoGen::Lto8)] {
        assert_eq!(
            feed(
                &mut f,
                medium_change(
                    MediumSpec {
                        family,
                        ..medium_spec()
                    },
                    1
                ),
                &Present
            )
            .unwrap_err(),
            FoldError::MediumFamilyChanged
        );
    }
    for source in [GenerationSource::Operator, GenerationSource::Label] {
        assert_eq!(
            feed(
                &mut f,
                medium_change(
                    MediumSpec {
                        usable_logical_bytes: u64::MAX,
                        source,
                        ..medium_spec()
                    },
                    1
                ),
                &Present
            )
            .unwrap_err(),
            FoldError::CapacityIncreaseRequiresMeasurement
        );
    }
    // A decrease is valid even though the old provenance was measured.
    let reduced = MediumSpec {
        usable_logical_bytes: 100,
        source: GenerationSource::Label,
        ..medium_spec()
    };
    feed(&mut f, medium_change(reduced.clone(), 1), &Present).unwrap();
    assert_eq!(f.tapes[&tape(1).uuid].medium, reduced);
    assert_eq!(
        feed(
            &mut f,
            medium_change(
                MediumSpec {
                    source: GenerationSource::Operator,
                    ..medium_spec()
                },
                2
            ),
            &Present
        )
        .unwrap_err(),
        FoldError::CapacityIncreaseRequiresMeasurement
    );
    // Equal capacity needs no measurement; a new measured revision may increase it.
    feed(&mut f, medium_change(reduced, 2), &Present).unwrap();
    let raised = MediumSpec {
        usable_logical_bytes: u64::MAX,
        ..medium_spec()
    };
    let update = medium_change(raised.clone(), 3);
    feed(&mut f, update.clone(), &Present).unwrap();
    assert_eq!(f.tapes[&tape(1).uuid].medium, raised);
    assert_eq!(f.tapes[&tape(1).uuid].media_revision, 4);
    assert!(matches!(
        feed(&mut f, update, &Present),
        Ok(fold::Validated::Duplicate(_))
    ));
    assert_eq!(
        feed(&mut f, medium_change(medium_spec(), 1), &Present).unwrap_err(),
        FoldError::GenerationMismatch
    );
    assert_eq!(
        feed(
            &mut f,
            medium_change(
                MediumSpec {
                    record_size: 0,
                    ..medium_spec()
                },
                4
            ),
            &Present
        )
        .unwrap_err(),
        FoldError::InvalidSchema
    );
}
/// Reserved codes round-trip through the codec and cannot open a fence, even on a blocked scope.
#[test]
fn reserved_fences_decode_then_refuse() {
    for model in [false, true] {
        for queued in [false, true] {
            let mut f = LibraryFold::default();
            feed(&mut f, provision(1, None), &Present).unwrap();
            if queued {
                let pending = record(
                    Body::SupersedeTail(SupersedeTail {
                        at: at(1, 9),
                        operation: [7; 16],
                        session_id: [3; 16],
                        preview_digest: [0; 32],
                        tail_file_count: 1,
                    }),
                    2,
                );
                f.receive(
                    2,
                    pending.clone(),
                    receipt(&pending, 2),
                    &View::default(),
                    &History::default(),
                )
                .unwrap();
                assert_eq!(f.pending.len(), 1);
            }
            let mut b = reserved_fence(model);
            if queued {
                let Body::FenceRaised(v) = &mut b else {
                    unreachable!()
                };
                v.operation = Some([7; 16]);
            }
            let r = record(b, f.received_through + 1);
            let mut bytes = Vec::new();
            wire::encode(&r, &mut bytes).unwrap();
            let decoded = wire::decode(r.body.tag() as u16, &bytes).unwrap();
            assert_eq!(decoded, r);
            let before = f.clone();
            assert_eq!(
                f.validate(&decoded, &Present, &History::default())
                    .unwrap_err(),
                FoldError::ReservedNotYetUsed
            );
            assert_eq!(
                f.receive(
                    f.received_through + 1,
                    decoded.clone(),
                    receipt(&decoded, f.received_through + 1),
                    &Present,
                    &History::default()
                ),
                Err(FoldError::ReservedNotYetUsed)
            );
            f.assert_same_state(&before);
        }
    }
}
