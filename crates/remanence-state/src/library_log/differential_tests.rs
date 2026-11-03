//! Differential oracle for live validation/publication and byte-decoded replay.
//! Native-unavailable histories cannot be appended live: those records bootstrap
//! both folds through replay. Every accepted candidate uses the post-publication apply_validated
//! path on the live side, including fences intersecting that bootstrapped queue.
use super::{fold::*, store::Receipt, tests::History, *};

/// Match the store's pre-append resolution on both paths as native proof advances.
fn resolve_pair(
    live: &mut LibraryFold,
    replay: &mut LibraryFold,
    view: &dyn TapeView,
    history: &History,
) {
    live.resolve(view, history).unwrap();
    replay.resolve(view, history).unwrap();
    live.assert_same_state(replay);
}

/// Port the reviewer's contradicted-evidence case and its scope/queued variants.
/// A native contradiction stays isolated; a fence must not pin the operation.
#[test]
fn contradicted_fences_release_other_scopes_live_and_replay() {
    use super::tests::*;
    for scope_tape in [1, 2] {
        for queued in [false, true] {
            let mut live = LibraryFold::default();
            let mut replay = LibraryFold::default();
            let mut history = History::default();
            let contradiction = View(std::collections::BTreeMap::from([(
                (tape(1).uuid, 9),
                TapeLookup::Contradiction,
            )]));
            let absent = View::default();
            let initial: &dyn TapeView = if queued { &absent } else { &contradiction };
            let op = |previous, event| {
                Body::OperationRecorded(OperationRecorded {
                    operation: [9; 16],
                    kind: OperationKind::MoveMedium,
                    previous,
                    event,
                })
            };
            let bodies = vec![
                provision(1, Some("A")),
                provision(2, Some("B")),
                op(None, OperationEvent::Started(Unit)),
                Body::SupersedeTail(SupersedeTail {
                    at: at(1, 9),
                    operation: [7; 16],
                    session_id: [3; 16],
                    preview_digest: [0; 32],
                    tail_file_count: 1,
                }),
                Body::FenceRaised(FenceRaised {
                    kind: FenceKind::Write,
                    scope: FenceScope::Tape(tape(scope_tape)),
                    cause: FenceCause::TransferError,
                    evidence: None,
                    at: Some(at(1, 9)),
                    operation: Some([9; 16]),
                }),
                Body::MediumSpecSet(MediumSpecSet {
                    tape: tape(2),
                    medium: medium_spec(),
                    change: GenerationChange {
                        expected: 1,
                        next: 2,
                    },
                }),
                op(
                    Some(record(diagnostic(), 3).reference(3)),
                    OperationEvent::Finished(Response {
                        response_fingerprint: None,
                    }),
                ),
            ];
            for (i, body) in bodies.into_iter().enumerate() {
                let n = i as u64 + 1;
                let view: &dyn TapeView = if n < 6 { initial } else { &contradiction };
                resolve_pair(&mut live, &mut replay, view, &history);
                if n == 6 {
                    assert!(live.pending.is_empty());
                    assert_eq!(live.pending_bytes(), 0);
                    assert_eq!(live.applied_through, 5);
                }
                let r = record(body, n);
                if n >= 5 {
                    assert!(matches!(
                        live.validate(&r, view, &history),
                        Ok(Validated::Apply)
                    ));
                }
                step(
                    &mut live,
                    &mut replay,
                    r.clone(),
                    receipt(&r, n),
                    view,
                    &history,
                )
                .unwrap();
                history.0.insert(n, r);
                if n == 5 {
                    assert_eq!(live.open_fences(), 1);
                    assert!(live.unresolved_fence_evidence.contains(&5));
                    assert_eq!(
                        live.pending.len(),
                        if queued {
                            1 + usize::from(scope_tape == 1)
                        } else {
                            0
                        }
                    );
                }
            }
            assert_eq!(
                live.contradictions.get(&Scope::Tape(tape(1).uuid)),
                Some(&FoldError::TapeContradiction)
            );
            assert_eq!(live.contradictions.len(), 1);
            assert_eq!(live.tapes[&tape(2).uuid].media_revision, 2);
            assert_eq!(live.open_fences(), 1);
            assert!(live.pending.is_empty());
            assert_eq!(live.applied_through, 7);
        }
    }
}

/// Native proof catches up before record 6, draining a decision, its successor
/// and a deferred fence before the next live append and byte-decoded replay step.
#[test]
fn tape_view_advances_mid_history_live_and_replay() {
    use super::tests::*;
    let mut live = LibraryFold::default();
    let mut replay = LibraryFold::default();
    let mut history = History::default();
    let absent = View::default();
    let bodies = vec![
        provision(1, None),
        Body::SupersedeTail(SupersedeTail {
            at: at(1, 9),
            operation: [7; 16],
            session_id: [3; 16],
            preview_digest: [0; 32],
            tail_file_count: 1,
        }),
        pool(1, 1),
        fence(1),
        diagnostic(),
        pool(1, 2),
    ];
    for (i, body) in bodies.into_iter().enumerate() {
        let n = i as u64 + 1;
        let view: &dyn TapeView = if n < 6 { &absent } else { &Present };
        if n == 6 {
            assert_eq!(live.pending.len(), 3);
            assert_eq!(live.applied_through, 1);
        }
        resolve_pair(&mut live, &mut replay, view, &history);
        if n == 6 {
            assert!(live.pending.is_empty());
            assert_eq!(live.pending_bytes(), 0);
            assert_eq!(live.applied_through, 5);
            assert!(live.supersede(2).is_some());
            assert_eq!(live.tapes[&tape(1).uuid].assignment_generation, 2);
        }
        let r = record(body, n);
        step(
            &mut live,
            &mut replay,
            r.clone(),
            receipt(&r, n),
            view,
            &history,
        )
        .unwrap();
        history.0.insert(n, r);
    }
    assert_eq!(live.tapes[&tape(1).uuid].assignment_generation, 3);
    assert_eq!(live.applied_through, 6);
    assert_eq!(live.open_fences(), 1);
    assert!(live.contradictions.is_empty());
}

/// A contradicted ordinary record may remain queued, but the fence must release
/// its operation scope even when that earlier record still blocks the tape.
#[test]
fn contradicted_fence_drains_past_an_ordinary_blocker() {
    use super::tests::*;
    let mut live = LibraryFold::default();
    let mut replay = LibraryFold::default();
    let mut history = History::default();
    let absent = View::default();
    let op = |previous, event| {
        Body::OperationRecorded(OperationRecorded {
            operation: [9; 16],
            kind: OperationKind::MoveMedium,
            previous,
            event,
        })
    };
    let mut raised = fence(1);
    if let Body::FenceRaised(v) = &mut raised {
        v.operation = Some([9; 16]);
    }
    for (i, body) in [
        provision(1, None),
        op(None, OperationEvent::Started(Unit)),
        Body::SupersedeTail(SupersedeTail {
            at: at(1, 9),
            operation: [7; 16],
            session_id: [3; 16],
            preview_digest: [0; 32],
            tail_file_count: 1,
        }),
        pool(1, 1),
        raised,
    ]
    .into_iter()
    .enumerate()
    {
        let n = i as u64 + 1;
        resolve_pair(&mut live, &mut replay, &absent, &history);
        let r = record(body, n);
        step(
            &mut live,
            &mut replay,
            r.clone(),
            receipt(&r, n),
            &absent,
            &history,
        )
        .unwrap();
        history.0.insert(n, r);
    }
    assert_eq!(live.pending.len(), 3);
    let queued_bytes = live.pending_bytes();
    let contradiction = View(std::collections::BTreeMap::from([(
        (tape(1).uuid, 9),
        TapeLookup::Contradiction,
    )]));
    resolve_pair(&mut live, &mut replay, &contradiction, &history);
    assert_eq!(live.pending.len(), 1);
    assert_eq!(live.pending[0].sequence, 4);
    assert!(live.pending_bytes() > 0 && live.pending_bytes() < queued_bytes);
    assert_eq!(live.open_fences(), 1);
    assert_eq!(live.contradictions.len(), 1);
    let r = record(
        op(
            Some(record(diagnostic(), 2).reference(2)),
            OperationEvent::Finished(Response {
                response_fingerprint: None,
            }),
        ),
        6,
    );
    assert!(matches!(
        live.validate(&r, &contradiction, &history),
        Ok(Validated::Apply)
    ));
    step(
        &mut live,
        &mut replay,
        r.clone(),
        receipt(&r, 6),
        &contradiction,
        &history,
    )
    .unwrap();
    assert_eq!(live.pending.len(), 1);
    assert_eq!(live.applied_through, 3);
}

/// Compare each transition, including refusals, duplicates, and all retained indexes.
pub(super) fn step(
    live: &mut LibraryFold,
    replay: &mut LibraryFold,
    record: Record,
    receipt: Receipt,
    view: &dyn TapeView,
    history: &History,
) -> Result<(), FoldError> {
    let n = receipt.sequence;
    let (bytes, _) = codec::encode(n, [0; 32], &record).unwrap();
    let mut decoder = codec::Decoder::at(bytes.as_slice(), n, 0);
    let frame = decoder.next_frame().unwrap().unwrap();
    codec::verify(&frame, [0; 32]).unwrap();
    let before = live.clone();
    let validation = live.validate(&record, view, history);
    before.assert_same_state(live); // validate is pure except for probe counters.
    let replay_result = replay.receive(n, frame.record, receipt.clone(), view, history);
    match validation {
        Ok(Validated::Apply) => {
            replay_result?;
            live.apply_validated(n, record, receipt, view)?;
        }
        Ok(Validated::Duplicate(original)) => {
            replay_result?;
            // A store retry writes nothing. Existing duplicate bytes count only on
            // replay; apply exposes the same bookkeeping for a source sequence.
            let result = live.apply(n, record, receipt, view, history)?;
            assert!(matches!(result, Validated::Duplicate(r) if r == original));
        }
        Err(error) => match replay_result {
            Ok(()) => {
                assert!(
                    matches!(error, FoldError::Unresolved | FoldError::TapeContradiction),
                    "live refused {error:?}, but replay accepted sequence {n}"
                );
                // Only unavailable native proof / scope contradictions can make a
                // valid persisted record unappendable at restart. Never bootstrap
                // UnknownTape or another library error: that would mask N4.
                live.receive(n, record, receipt, view, history)?;
            }
            Err(replay_error) => {
                assert_eq!(error, replay_error);
                live.assert_same_state(replay);
                return Err(error);
            }
        },
    }
    live.assert_same_state(replay);
    Ok(())
}

/// Both rare lifetime decisions preserve uniqueness after terminal operation pruning.
#[test]
fn n1_unprotected_acceptance_survives_operation_terminal() {
    use super::tests::*;
    let op = |previous, event| {
        Body::OperationRecorded(OperationRecorded {
            operation: [7; 16],
            kind: OperationKind::MoveMedium,
            previous,
            event,
        })
    };
    let mut f = LibraryFold::default();
    feed(&mut f, provision(1, None), &Present).unwrap();
    feed(&mut f, op(None, OperationEvent::Started(Unit)), &Present).unwrap();
    let a = AcceptUnprotectedFinalization {
        at: TapeAt {
            tape: tape(1),
            sequence: 9,
            record_digest: [5; 32],
        },
        operation: [7; 16],
        w: 1,
        t: 2,
        reason: "operator".into(),
    };
    feed(
        &mut f,
        Body::AcceptUnprotectedFinalization(a.clone()),
        &Present,
    )
    .unwrap();
    feed(
        &mut f,
        op(
            Some(record(diagnostic(), 2).reference(2)),
            OperationEvent::Finished(Response {
                response_fingerprint: None,
            }),
        ),
        &Present,
    )
    .unwrap();
    assert!(
        matches!(feed(&mut f, Body::AcceptUnprotectedFinalization(a.clone()), &Present), Ok(Validated::Duplicate(receipt)) if receipt.sequence == 3)
    );
    let mut conflict = a;
    conflict.reason = "changed".into();
    assert_eq!(
        feed(
            &mut f,
            Body::AcceptUnprotectedFinalization(conflict),
            &Present
        )
        .unwrap_err(),
        FoldError::ConflictingDuplicate
    );
}
