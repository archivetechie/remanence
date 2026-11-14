//! Ruling 21 reconciles only durably named lost tape fences before a runtime exists.
//! Native endings supply conservative evidence; applied library fences supply cover.
use super::*;
use crate::tape_log::{Record, SessionEndReason, SessionEnded, Tag};
use std::collections::BTreeMap;

fn failed_ending(end: &SessionEnded) -> bool {
    matches!(
        end.reason,
        SessionEndReason::FailedAppend
            | SessionEndReason::DriveError
            | SessionEndReason::CloseoutFailed
    ) || end.tail_file_count > 0
        || end.possible_tail_blocks > 0
}

fn invalid(message: &str) -> RuntimeError {
    StateError::JournalReplayFailed(format!("lost fence reconciliation: {message}")).into()
}

/// The caller retains the state lease and authority lock until projection and construction.
pub(super) fn reconcile(
    authority: &LibraryAuthority,
    view: &NativeTapeView,
) -> Result<ReconciliationSummary, RuntimeError> {
    let mut outcomes = Vec::new();
    let mut tapes = authority.fence_reconciliation_tapes()?;
    tapes.sort_unstable();
    tapes.dedup();
    let mut fences: BTreeMap<lib::Id, Vec<(u64, lib::FenceRaised)>> = BTreeMap::new();
    authority
        .snapshot()?
        .replay(view, &mut |applied| {
            if let lib::BodyView::FenceRaised(fence) = applied.record().body_view() {
                if let lib::FenceScope::Tape(tape) = fence.scope {
                    if tapes.binary_search(&tape.uuid).is_err() {
                        return Ok(());
                    }
                    fences
                        .entry(tape.uuid)
                        .or_default()
                        .push((applied.sequence(), fence.clone()));
                }
            }
            Ok(())
        })
        .map_err(AuthorityError::from)?;
    for tape in tapes {
        let Some(native) = view.snapshots().find(|s| s.header().tape_uuid == tape) else {
            tracing::warn!(tape = %uuid::Uuid::from_bytes(tape), "lost fence skipped: no native snapshot");
            outcomes.push((tape, "skipped: no native snapshot"));
            continue;
        };
        let summary = native.summary();
        if summary.open_session.is_some() {
            tracing::warn!(tape = %uuid::Uuid::from_bytes(tape), "lost fence skipped: native session still open");
            outcomes.push((tape, "skipped: native session still open"));
            continue;
        }
        let Some(end) = summary
            .last_session_end
            .as_ref()
            .filter(|end| failed_ending(end))
        else {
            tracing::warn!(tape = %uuid::Uuid::from_bytes(tape), "lost fence skipped: no eligible ending");
            outcomes.push((tape, "skipped: no eligible ending"));
            continue;
        };
        let Some(state) = authority
            .tape_state(tape)
            .filter(|s| !s.retired && s.tape.incarnation == end.incarnation)
        else {
            tracing::warn!(tape = %uuid::Uuid::from_bytes(tape), "lost fence skipped: absent, retired or different library incarnation");
            outcomes.push((
                tape,
                "skipped: absent, retired or different library incarnation",
            ));
            continue;
        };
        let mut opened = None;
        let mut ending = None;
        native
            .visit_frames(|frame| {
                match &frame.record {
                    Record::SessionOpened(value) if value.session_id == end.session_id => {
                        opened = Some(frame.sequence)
                    }
                    Record::SessionEnded(value) if value.session_id == end.session_id => {
                        ending = Some((frame.sequence, opened.unwrap_or(frame.sequence)))
                    }
                    _ => (),
                }
                Ok(())
            })
            .map_err(StateError::from)?;
        let (s_e, s_o) = ending.ok_or_else(|| invalid("summary ending has no frame"))?;
        let evidence = native
            .record_evidence(s_e)
            .map_err(StateError::from)?
            .filter(|e| e.kind == Tag::SessionEnded)
            .ok_or_else(|| invalid("ending evidence is missing or has the wrong kind"))?;
        let mut covered = false;
        for (sequence, fence) in fences.get(&tape).into_iter().flatten() {
            if authority.active_fence(*sequence).is_some() {
                covered = true;
                break;
            }
            if let Some(at) = &fence.at {
                if at.tape.uuid == tape
                    && at.sequence >= s_o
                    && native
                        .record_evidence(at.sequence)
                        .map_err(StateError::from)?
                        .is_some_and(|e| e.digest == at.record_digest)
                {
                    covered = true;
                    break;
                }
            }
        }
        if covered {
            outcomes.push((tape, "covered"));
            continue;
        }
        #[cfg(test)]
        cut(1)?;
        authority.fence_raised(
            lib::FenceRaised {
                kind: lib::FenceKind::Write,
                scope: lib::FenceScope::Tape(state.tape),
                cause: lib::FenceCause::AuthorityAmbiguous,
                evidence: None,
                at: Some(lib::TapeAt {
                    tape: state.tape,
                    sequence: s_e,
                    record_digest: evidence.digest,
                }),
                operation: None,
                detail: lib::Bounded(vec![]),
            },
            lib::AuditActor::System,
            lib::SourceLayer::Layer4,
            view,
        )?;
        outcomes.push((tape, "raised"));
        #[cfg(test)]
        cut(2)?;
    }
    authority.finish_fence_reconciliation()?;
    Ok(outcomes)
}

#[cfg(test)]
thread_local! { static CUT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
fn cut(stage: u8) -> Result<(), RuntimeError> {
    if CUT.get() == stage {
        Err(invalid("injected open cut"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "native_fences_tests.rs"]
mod tests;
