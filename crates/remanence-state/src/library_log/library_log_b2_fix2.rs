//! Second-round public fold regressions, ported from the reviewer scratch cases.
use crate::library_log::{fold::*, store::Receipt, *};
struct Present;
impl TapeView for Present {
    fn lookup(&self, a: &TapeAt) -> Result<TapeLookup, FoldError> {
        Ok(TapeLookup::Present {
            kind: crate::tape_log::Tag::FinalizationPrepared,
            digest: a.record_digest,
        })
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
fn provision(n: u8) -> Body {
    Body::Reprovision(Reprovision {
        pool_source: None,
        operation: None,
        replaced_bot: None,
        tape: tape(n),
        predecessor: None,
        mode: ProvisionMode::Initialize,
        label: None,
        label_provenance: None,
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
fn feed(f: &mut LibraryFold, b: Body) -> Result<Validated, FoldError> {
    let n = f.received_through + 1;
    let (r, rc) = rec(b, n);
    f.apply(n, r, rc, &Present, &NoHistory)
}
fn r(n: u64) -> DecisionRef {
    DecisionRef {
        sequence: n,
        record_uuid: uuid(n),
    }
}
fn op(id: Id, previous: Option<DecisionRef>, event: OperationEvent) -> Body {
    Body::OperationRecorded(OperationRecorded {
        operation: id,
        kind: OperationKind::MoveMedium,
        previous,
        event,
    })
}
fn accepted(key: Option<Id>, fp: u8) -> OperationEvent {
    OperationEvent::Accepted(Accepted {
        actor_fingerprint: "actor".into(),
        idempotency_key: key,
        request_fingerprint: [fp; 32],
        subject: None,
    })
}
fn finished() -> OperationEvent {
    OperationEvent::Finished(Response {
        response_fingerprint: None,
    })
}

/// N1: the native tape fold's supersede(sequence) lookup after the operation that recorded it finishes.
#[test]
fn n1_supersede_lookup_survives_operation_terminal() {
    let mut f = LibraryFold::default();
    feed(&mut f, provision(1)).unwrap(); // 1
    feed(&mut f, op([7; 16], None, OperationEvent::Started(Unit))).unwrap(); // 2
    let st = SupersedeTail {
        at: TapeAt {
            tape: tape(1),
            sequence: 9,
            record_digest: [5; 32],
        },
        operation: Some([7; 16]),
        session_id: Some([3; 16]),
        preview_digest: Some([0; 32]),
        tail_file_count: 1,
    };
    feed(&mut f, Body::SupersedeTail(st.clone())).unwrap(); // 3
    assert!(f.supersede(3).is_some(), "visible while operation open");
    feed(&mut f, op([7; 16], Some(r(2)), finished())).unwrap(); // 4
    eprintln!(
        "N1 supersede(3) after Finished: {:?}",
        f.supersede(3).is_some()
    );
    let again = feed(&mut f, Body::SupersedeTail(st.clone()));
    eprintln!("N1 second SupersedeTail for same (tape, session) after Finished: {again:?}");
    assert!(
        f.supersede(3).is_some(),
        "supersede(3) lost after its operation finished"
    );
    assert!(matches!(again, Ok(Validated::Duplicate(receipt)) if receipt.sequence == 3));
    let mut conflict = st;
    conflict.operation = Some([8; 16]);
    assert_eq!(
        feed(&mut f, Body::SupersedeTail(conflict)).unwrap_err(),
        FoldError::ConflictingDuplicate
    );
}

/// N2: a keyed operation, once terminal, is re-opened by an identical Accepted retry.
#[test]
fn n2_identical_keyed_retry_after_terminal() {
    let mut f = LibraryFold::default();
    feed(&mut f, op([7; 16], None, accepted(Some([4; 16]), 6))).unwrap(); // 1
    feed(&mut f, op([7; 16], Some(r(1)), finished())).unwrap(); // 2
    let retry = feed(&mut f, op([7; 16], None, accepted(Some([4; 16]), 6)));
    eprintln!("N2 identical keyed Accepted after Finished: {retry:?}");
    let other_fp = f.validate(
        &rec(
            op([7; 16], None, accepted(Some([4; 16]), 9)),
            f.received_through + 1,
        )
        .0,
        &Present,
        &NoHistory,
    );
    eprintln!("N2 different fingerprint after terminal: {other_fp:?}");
    let other_op = f.validate(
        &rec(
            op([8; 16], None, accepted(Some([4; 16]), 6)),
            f.received_through + 1,
        )
        .0,
        &Present,
        &NoHistory,
    );
    eprintln!("N2 different operation same key after terminal: {other_op:?}");
    assert_eq!(
        retry.unwrap_err(),
        FoldError::ClosedOperationReplay { operation: [7; 16] }
    );
    assert_eq!(other_fp.unwrap_err(), FoldError::ConflictingDuplicate);
    assert_eq!(other_op.unwrap_err(), FoldError::ConflictingDuplicate);
}

/// N6: the approved memory bound deliberately does not detect fresh unkeyed id reuse.
#[test]
fn n6_unkeyed_id_reuse_is_not_detected() {
    let mut f = LibraryFold::default();
    feed(&mut f, op([7; 16], None, OperationEvent::Started(Unit))).unwrap(); // 1
    feed(&mut f, op([7; 16], Some(r(1)), finished())).unwrap(); // 2
    let again = feed(&mut f, op([7; 16], None, OperationEvent::Started(Unit)));
    eprintln!("N3 Started with previous=None after Finished: {again:?}");
    let again2 = feed(&mut f, op([7; 16], Some(r(3)), finished()));
    eprintln!("N3 second Finished: {again2:?}");
    assert!(matches!(again, Ok(Validated::Apply)));
    assert!(matches!(again2, Ok(Validated::Apply)));
}

mod live_fences {
    use crate::library_log::{fold::*, store::Receipt, *};
    struct Present;
    impl TapeView for Present {
        fn lookup(&self, a: &TapeAt) -> Result<TapeLookup, FoldError> {
            Ok(TapeLookup::Present {
                kind: crate::tape_log::Tag::FinalizationPrepared,
                digest: a.record_digest,
            })
        }
    }
    struct NotYetView;
    impl TapeView for NotYetView {
        fn lookup(&self, _: &TapeAt) -> Result<TapeLookup, FoldError> {
            Ok(TapeLookup::NotYet)
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
        (
            r,
            Receipt {
                sequence: n,
                record_uuid: uuid(n),
                record_hash: [0; 32],
                synced: true,
            },
        )
    }
    fn tape(n: u8) -> TapeRef {
        TapeRef {
            uuid: [n; 16],
            incarnation: 1,
        }
    }
    fn provision(n: u8, pred: Option<TapeRef>) -> Body {
        Body::Reprovision(Reprovision {
            pool_source: None,
            operation: None,
            replaced_bot: None,
            tape: tape(n),
            predecessor: pred,
            mode: ProvisionMode::Initialize,
            label: None,
            label_provenance: None,
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
    fn fence(t: TapeRef) -> Body {
        Body::FenceRaised(FenceRaised {
            detail: Bounded(vec![]),
            kind: FenceKind::Write,
            scope: FenceScope::Tape(t),
            cause: FenceCause::TransferError,
            evidence: None,
            at: None,
            operation: None,
        })
    }
    /// Replay the reviewer's queued history with the tape log behind, as at daemon start.
    fn queued() -> LibraryFold {
        let mut f = LibraryFold::default();
        let mut n = 0;
        for b in [
            provision(1, None),
            Body::SupersedeTail(SupersedeTail {
                at: TapeAt {
                    tape: tape(1),
                    sequence: 9,
                    record_digest: [5; 32],
                },
                operation: Some([3; 16]),
                session_id: Some([3; 16]),
                preview_digest: Some([0; 32]),
                tail_file_count: 1,
            }),
            Body::Retire(Retire {
                tape: tape(1),
                reason: RetireReason::OperatorRequested,
                observed_tape_sequence: None,
                assignment_generation: 1,
                detail: None,
            }),
            provision(2, Some(tape(1))),
        ] {
            n += 1;
            let (r, rc) = rec(b, n);
            f.receive(n, r, rc, &NotYetView, &NoHistory).unwrap();
        }
        f
    }
    /// Shape checks still run before a queued fence can restrict authority (N10).
    #[test]
    fn deferred_fence_rejects_zero_incarnation_and_wrong_scope() {
        let mut f = queued();
        let n = f.received_through + 1;
        let mut zero = tape(1);
        zero.incarnation = 0;
        let (record, receipt) = rec(fence(zero), n);
        assert_eq!(
            f.apply(n, record, receipt, &NotYetView, &NoHistory)
                .unwrap_err(),
            FoldError::InvalidSchema
        );
        let mut wrong = fence(tape(2));
        if let Body::FenceRaised(v) = &mut wrong {
            v.kind = FenceKind::DriveFault;
        }
        let (record, receipt) = rec(wrong, n);
        assert_eq!(
            f.apply(n, record.clone(), receipt.clone(), &NotYetView, &NoHistory)
                .unwrap_err(),
            FoldError::FenceScopeMismatch
        );
        assert_eq!(
            f.receive(n, record, receipt, &NotYetView, &NoHistory)
                .unwrap_err(),
            FoldError::FenceScopeMismatch
        );
        assert_eq!(f.received_through, n - 1);
        assert_eq!(f.open_fences(), 0);
    }

    /// N4: a live append of a fence on the successor, with the same queue, is refused.
    #[test]
    fn n4_live_fence_on_queued_successor() {
        let mut f = queued();
        let n = f.received_through + 1;
        let v = f.validate(&rec(fence(tape(2)), n).0, &NotYetView, &NoHistory);
        eprintln!("N4 live append validate(FenceRaised T2) with queue: {v:?}");
        assert!(matches!(v, Ok(Validated::Apply)));
        let (record, receipt) = rec(fence(tape(2)), n);
        f.apply(n, record, receipt, &NotYetView, &NoHistory)
            .unwrap();
        assert_eq!(f.open_fences(), 1);
        assert_eq!(f.pending.len(), 4);
        f.resolve(&Present, &NoHistory).unwrap();
        assert!(f.pending.is_empty());
        assert!(f.contradicted_tapes() == 0);
        assert_eq!(f.open_fences(), 1);
    }
    /// N5: live append vs replay of the same fence on a tape whose Retire is queued.
    #[test]
    fn n5_live_and_replay_diverge() {
        // Live: fence appended through validate+apply while Retire(T1) is queued.
        let mut live = queued();
        let n = live.received_through + 1;
        let (r, rc) = rec(fence(tape(1)), n);
        let v = live.apply(n, r.clone(), rc.clone(), &NotYetView, &NoHistory);
        eprintln!(
            "N5 live apply(FenceRaised T1): {v:?}; pending={}",
            live.pending.len()
        );
        assert!(matches!(v, Ok(Validated::Apply)));
        assert_eq!(live.open_fences(), 1);
        assert_eq!(live.pending.len(), 4);
        live.resolve(&Present, &NoHistory).unwrap();
        // Replay: the same bytes received after restart.
        let mut replay = queued();
        replay.receive(n, r, rc, &NotYetView, &NoHistory).unwrap();
        replay.resolve(&Present, &NoHistory).unwrap();
        eprintln!(
            "N5 live contradictions={:?} replay contradictions={:?}",
            live.contradictions, replay.contradictions
        );
        // D3: compare authority, not excluded-record diagnostic counters.
        assert_eq!(
            live.highest_reference(tape(1)),
            replay.highest_reference(tape(1))
        );
        assert_eq!(live.contradicted_tapes(), replay.contradicted_tapes());
        assert_eq!(live.contradicted_tapes(), 0);
        let expected_reports =
            std::collections::BTreeMap::from([(n, (tape(1), FoldError::RetiredIdentity))]);
        assert_eq!(live.fence_liveness_failures, expected_reports);
        assert_eq!(replay.fence_liveness_failures, expected_reports);
        assert_eq!(live.open_fences(), 1);
        assert_eq!(live.open_fences(), replay.open_fences());
        assert!(live.pending.is_empty());
        assert!(replay.pending.is_empty());
        assert_eq!(live.tapes, replay.tapes);
        assert_eq!(live.applied_through, replay.applied_through);
    }
}

/// Shared reads must remain possible for the B4b authority.
#[test]
fn library_fold_is_sync() {
    fn assert_sync<T: Sync>() {}
    assert_sync::<LibraryFold>();
}

/// The generated schema records the limits needed by B4b's operation writers.
#[test]
fn schema_lifetime_and_writer_contracts() {
    let schema = include_str!("../../../../fixtures/library-log/SCHEMA.md");
    for clause in [
        "ClosedOperationReplay",
        "reuse of a closed unkeyed operation id is not detected in fold state",
        "Tags 13, 14 and 26 require an OperationRecorded chain",
        "one entry per crash supersede or unprotected acceptance",
    ] {
        assert!(schema.contains(clause), "missing contract: {clause}");
    }
}
