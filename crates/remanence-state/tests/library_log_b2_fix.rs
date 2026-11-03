//! Public-API regressions ported from review-4b2-repro.rs.
use remanence_state::library_log::{fold::*, store::Receipt, *};
struct Present;
impl TapeView for Present {
    fn lookup(&self, a: &TapeAt) -> TapeLookup {
        TapeLookup::Present {
            kind: remanence_state::tape_log::Tag::FinalizationPrepared,
            digest: a.record_digest,
        }
    }
}
struct NotYetView;
impl TapeView for NotYetView {
    fn lookup(&self, _: &TapeAt) -> TapeLookup {
        TapeLookup::NotYet
    }
}
struct NoHistory;
impl LibraryHistory for NoHistory {
    fn lookup(&self, _: DecisionRef) -> Result<Option<Record>, FoldError> {
        Ok(None)
    }
    fn lookup_fence(&self, _: u64) -> Result<Option<Record>, FoldError> {
        Ok(None)
    }
}
fn uuid(n: u64) -> Id {
    let mut b = [0u8; 16];
    b[0] = 0xc0;
    b[8..].copy_from_slice(&n.to_be_bytes());
    b
}
fn rec(body: Body, n: u64) -> (Record, Receipt) {
    let r = Record {
        envelope: Envelope {
            record_uuid: uuid(n),
            timestamp_unix_ns: 1,
            host_id: "h".into(),
            process_id: 1,
            software_build: "b".into(),
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
        },
        body,
    };
    let rc = Receipt {
        sequence: n,
        record_uuid: uuid(n),
        record_hash: [0; 32],
        synced: true,
    };
    (r, rc)
}
fn tape(n: u8) -> TapeRef {
    TapeRef {
        uuid: [n; 16],
        incarnation: 1,
    }
}
fn provision(n: u8, pred: Option<TapeRef>) -> Body {
    Body::Reprovision(Reprovision {
        tape: tape(n),
        predecessor: pred,
        mode: ProvisionMode::Initialize,
        label: None,
        label_provenance: None,
        block_size: 1048576,
        parity_scheme: None,
        media: None,
        pool: None,
        endpoint: None,
        adoption: None,
    })
}
fn ev() -> CompletionEvidence {
    CompletionEvidence {
        dispatch: Dispatch::Issued,
        completion: Completion::Known,
        scsi_status: None,
        host_status: None,
        driver_status: None,
        sense: None,
        residual: None,
    }
}
fn feed(f: &mut LibraryFold, b: Body) -> Result<Validated, FoldError> {
    let n = f.received_through + 1;
    let (r, rc) = rec(b, n);
    f.apply(n, r, rc, &Present, &NoHistory)
}
fn endpoint() -> DriveKey {
    DriveKey::Bay(Bay {
        library_serial: "LIBA0001".into(),
        bay: 1,
    })
}

/// Finding: a second outcome for a resolved intent that has a parent operation is accepted.
#[test]
fn second_outcome_for_resolved_parented_intent() {
    let mut f = LibraryFold::default();
    feed(&mut f, provision(1, None)).unwrap();
    feed(
        &mut f,
        Body::MotionIntent(MotionIntent {
            command: [1; 16],
            parent_operation: Some([8; 16]),
            motion: Motion::Load(Load {
                endpoint: endpoint(),
                attempt: 1,
            }),
            tape: Some(tape(1)),
            inventory_generation: None,
            readiness_generation: None,
        }),
    )
    .unwrap();
    let intent = DecisionRef {
        sequence: 2,
        record_uuid: uuid(2),
    };
    let done = |result| {
        Body::MotionOutcome(MotionOutcome {
            intent,
            result,
            evidence: ev(),
            confirmation: Some(Confirmation::Readiness(1)),
        })
    };
    feed(&mut f, done(MotionResult::Done)).unwrap();
    assert!(!f.inventory_unknown(&Scope::Library("LIBA0001".into())));
    let second = feed(&mut f, done(MotionResult::Failed));
    eprintln!("second outcome after Done: {second:?}");
    let inv = feed(
        &mut f,
        Body::InventoryReconciled(InventoryReconciled {
            library_serial: "LIBA0001".into(),
            observation_generation: 1,
            resolves: Bounded(vec![intent]),
            complete: true,
            elements: Bounded(vec![]),
        }),
    );
    eprintln!("inventory resolving a Done intent: {inv:?}");
    assert_eq!(second.unwrap_err(), FoldError::OutcomeWithoutOpenIntent);
    assert_eq!(inv.unwrap_err(), FoldError::OutcomeWithoutOpenIntent);
}

/// Finding: FenceRaised is "never held", but its liveness check reads state that is queued.
#[test]
fn fence_on_tape_whose_provisioning_is_queued() {
    let mut f = LibraryFold::default();
    let mut n = 0;
    let mut recv = |f: &mut LibraryFold, b: Body| {
        n += 1;
        let (r, rc) = rec(b, n);
        f.receive(n, r, rc, &NotYetView, &NoHistory)
    };
    recv(&mut f, provision(1, None)).unwrap();
    recv(
        &mut f,
        Body::SupersedeTail(SupersedeTail {
            at: TapeAt {
                tape: tape(1),
                sequence: 9,
                record_digest: [5; 32],
            },
            operation: [3; 16],
            session_id: [3; 16],
            preview_digest: [0; 32],
            tail_file_count: 1,
        }),
    )
    .unwrap();
    recv(
        &mut f,
        Body::Retire(Retire {
            tape: tape(1),
            reason: RetireReason::OperatorRequested,
            observed_tape_sequence: None,
            assignment_generation: 1,
            detail: None,
        }),
    )
    .unwrap();
    recv(&mut f, provision(2, Some(tape(1)))).unwrap();
    eprintln!("pending after reprovision: {}", f.pending.len());
    let r = recv(
        &mut f,
        Body::FenceRaised(FenceRaised {
            kind: FenceKind::Write,
            scope: FenceScope::Tape(tape(2)),
            cause: FenceCause::TransferError,
            evidence: None,
            at: None,
            operation: None,
        }),
    );
    eprintln!("fence on the successor: {r:?}");
    r.unwrap();
    assert_eq!(f.open_fences(), 1);
    f.resolve(&Present, &NoHistory).unwrap();
    assert!(f.pending.is_empty());
    assert!(f.contradictions.is_empty());
}

struct Repaired;
impl TapeView for Repaired {
    fn lookup(&self, _: &TapeAt) -> TapeLookup {
        // The tape log now holds a different record (other kind, other digest) at that sequence.
        TapeLookup::Present {
            kind: remanence_state::tape_log::Tag::SessionEnded,
            digest: [0xee; 32],
        }
    }
}
/// Finding: a digest mismatch with a different kind stops all of replay instead of scoping the tape.
#[test]
fn repaired_tape_log_kind_change_is_not_scoped() {
    let mut f = LibraryFold::default();
    let mut n = 0;
    let mut recv = |f: &mut LibraryFold, b: Body| {
        n += 1;
        let (r, rc) = rec(b, n);
        f.receive(n, r, rc, &Repaired, &NoHistory)
    };
    recv(&mut f, provision(1, None)).unwrap();
    let r = recv(
        &mut f,
        Body::ManualFinalizationAccepted(ManualFinalizationAccepted {
            at: TapeAt {
                tape: tape(1),
                sequence: 4,
                record_digest: [5; 32],
            },
            operation: [3; 16],
            actor_fingerprint: "op".into(),
            idempotency_key: [4; 16],
            request_fingerprint: [6; 32],
            assignment_generation: 1,
        }),
    );
    eprintln!(
        "replay of tag 15 against a repaired tape log: {r:?}; contradictions={}",
        f.contradictions.len()
    );
    r.unwrap();
    assert_eq!(
        f.contradictions.get(&Scope::Tape(tape(1).uuid)),
        Some(&FoldError::TapeContradiction)
    );
    recv(&mut f, provision(2, None)).unwrap();
    assert!(f.tapes.contains_key(&tape(2).uuid));
}

/// Deferred validation fails closed on its tape, while leaving unrelated replay live.
#[test]
fn deferred_fence_liveness_failure_is_scoped() {
    let mut f = LibraryFold::default();
    let mut n = 0;
    let mut recv = |f: &mut LibraryFold, b: Body| {
        n += 1;
        let (r, rc) = rec(b, n);
        f.receive(n, r, rc, &NotYetView, &NoHistory)
    };
    recv(&mut f, provision(1, None)).unwrap();
    recv(
        &mut f,
        Body::SupersedeTail(SupersedeTail {
            at: TapeAt {
                tape: tape(1),
                sequence: 9,
                record_digest: [5; 32],
            },
            operation: [3; 16],
            session_id: [3; 16],
            preview_digest: [0; 32],
            tail_file_count: 1,
        }),
    )
    .unwrap();
    recv(
        &mut f,
        Body::Retire(Retire {
            tape: tape(1),
            reason: RetireReason::OperatorRequested,
            observed_tape_sequence: None,
            assignment_generation: 1,
            detail: None,
        }),
    )
    .unwrap();
    recv(
        &mut f,
        Body::FenceRaised(FenceRaised {
            kind: FenceKind::Write,
            scope: FenceScope::Tape(tape(1)),
            cause: FenceCause::TransferError,
            evidence: None,
            at: None,
            operation: None,
        }),
    )
    .unwrap();
    assert_eq!(f.open_fences(), 1);
    f.resolve(&Present, &NoHistory).unwrap();
    assert_eq!(
        f.contradictions.get(&Scope::Tape(tape(1).uuid)),
        Some(&FoldError::RetiredIdentity)
    );
    assert!(f.pending.is_empty());
    assert_eq!(f.open_fences(), 1);
    recv(&mut f, provision(2, None)).unwrap();
    assert!(f.tapes.contains_key(&tape(2).uuid));
}
