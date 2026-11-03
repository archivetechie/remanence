//! Differential oracle for live validation/publication and byte-decoded replay.
//! Native-unavailable histories cannot be appended live: those records bootstrap
//! both folds through replay. Every accepted candidate uses the post-publication apply_validated
//! path on the live side, including fences intersecting that bootstrapped queue.
use super::{fold::*, store::Receipt, tests::History, *};

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
            live.apply_validated(n, record, receipt, view);
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
