//! Ruling 31's single evaluator for received native floors and pre-rebuild SQL
//! marks. Observations are ordered and independent; only exact typed fence
//! evidence carries an obligation. This module never writes sources or SQL.
use crate::library_log::{self as lib, native_view::NativeTapeView, TapeView};
use std::collections::{BTreeMap, BTreeSet};

/// Snapshot the tape-local cause-14 membership once per native projection pass.
pub(super) fn fenced_tapes(fold: &lib::LibraryFold) -> BTreeSet<lib::Id> {
    fold.tapes
        .keys()
        .filter(|id| {
            fold.active_tape_fences(id)
                .any(|(_, fence)| fence.cause == lib::FenceCause::NativeLogBehindProjection)
        })
        .copied()
        .collect()
}

/// Until L1b introduces forced release, cause-14 evidence cannot be acknowledged away.
pub(super) fn refuse_unforced_release(fence: &lib::FenceRaised) -> Result<(), crate::StateError> {
    if fence.cause == lib::FenceCause::NativeLogBehindProjection {
        return Err(crate::StateError::ConfigInvalid("native_log_behind_projection fence requires --force: restore the exact tape log, then release with --force; or rebuild the catalog with rem rebuild-catalog-from-journals, then release with --force; or retire the tape".into()));
    }
    Ok(())
}

/// Validated extent of the frozen native source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct NativeExtent {
    pub last_closed: u64,
    pub closed_end: u64,
}
/// The two SQL rows are one obligation; fixture-only marks may lack a byte binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MarkEvidence {
    pub sequence: u64,
    pub offset: u64,
    pub source: Option<(u64, lib::Hash)>,
}

fn extent(view: &NativeTapeView, tape: lib::Id) -> Option<NativeExtent> {
    view.snapshot(tape).map(|s| NativeExtent {
        last_closed: s.summary().last_closed_sequence,
        closed_end: s.summary().closed_end,
    })
}

/// Test in ruling order, so every unsatisfied obligation has exactly one word.
fn observe_floor(
    floor: (u64, lib::Hash, u64),
    digests: &BTreeSet<lib::Hash>,
    native: Option<NativeExtent>,
    lookup: impl FnOnce() -> Result<lib::TapeLookup, lib::FoldError>,
) -> Option<&'static str> {
    let Some(native) = native else {
        return Some("absent");
    };
    if digests.len() > 1 {
        return Some("digest_conflict");
    }
    if native.last_closed < floor.0 {
        return Some("behind");
    }
    match lookup() {
        Ok(lib::TapeLookup::Present { digest, .. }) if floor.0 > 0 && digest == floor.1 => None,
        Ok(lib::TapeLookup::EmptyPrefix { digest }) if floor.0 == 0 && digest == floor.1 => None,
        _ => Some("digest_mismatch"),
    }
}
fn observe_mark(
    mark: &MarkEvidence,
    native: Option<NativeExtent>,
    digest: impl FnOnce(u64) -> Result<lib::Hash, ()>,
) -> Option<&'static str> {
    let Some(native) = native else {
        return Some("absent");
    };
    if native.last_closed < mark.sequence {
        return Some("behind");
    }
    if let Some((end, expected)) = mark.source {
        if native.closed_end < end {
            return Some("short_bytes");
        }
        if digest(end) != Ok(expected) {
            return Some("digest_mismatch");
        }
    }
    None
}
fn detail(raise: &lib::FenceRaised, name: &str) -> Option<lib::DiagnosticValue> {
    raise
        .detail
        .0
        .iter()
        .find(|e| e.0.as_str() == name)
        .map(|e| e.1.clone())
}
fn carries_floor(raise: &lib::FenceRaised, floor: (u64, lib::Hash, u64)) -> bool {
    detail(raise, "floor_sequence") == Some(lib::DiagnosticValue::Unsigned(floor.0))
        && detail(raise, "floor_digest")
            == Some(lib::DiagnosticValue::Bytes(lib::Blob(floor.1.to_vec())))
}
fn carries_mark(raise: &lib::FenceRaised, mark: &MarkEvidence) -> bool {
    if detail(raise, "mark_sequence") != Some(lib::DiagnosticValue::Unsigned(mark.sequence))
        || detail(raise, "mark_offset") != Some(lib::DiagnosticValue::Unsigned(mark.offset))
    {
        return false;
    }
    match mark.source {
        Some((end, digest)) => {
            detail(raise, "source_byte_end") == Some(lib::DiagnosticValue::Unsigned(end))
                && detail(raise, "source_digest")
                    == Some(lib::DiagnosticValue::Bytes(lib::Blob(digest.to_vec())))
        }
        None => {
            detail(raise, "source_byte_end").is_none() && detail(raise, "source_digest").is_none()
        }
    }
}
/// Offline retention is exactly the mark carry test, including the offset.
pub(crate) fn mark_retained(fold: &lib::LibraryFold, tape: &lib::Id, mark: &MarkEvidence) -> bool {
    fold.active_tape_fences(tape).any(|(_, raise)| {
        raise.cause == lib::FenceCause::NativeLogBehindProjection && carries_mark(raise, mark)
    })
}
/// An unsatisfied received floor with its digest evidence and observation.
type FloorObligation<'a> = ((u64, lib::Hash, u64), &'a BTreeSet<lib::Hash>, &'static str);

fn raise(
    tape: lib::TapeRef,
    native: Option<NativeExtent>,
    floor: Option<FloorObligation<'_>>,
    mark: Option<(&MarkEvidence, &'static str)>,
) -> lib::FenceRaised {
    use lib::DiagnosticValue::{Bytes, Text, Unsigned};
    let mut detail = BTreeMap::<lib::Text, lib::DiagnosticValue>::new();
    let basis = match (floor.is_some(), mark.is_some()) {
        (true, true) => "both",
        (true, false) => "floor",
        (false, true) => "mark",
        (false, false) => unreachable!("a raise carries an unsatisfied obligation"),
    };
    detail.insert("basis".into(), Text(basis.into()));
    let mut at = None;
    if let Some(((sequence, digest, named_by), digests, observed)) = floor {
        if observed == "digest_conflict" {
            detail.insert(
                "conflict_digests".into(),
                Bytes(lib::Blob(
                    digests.iter().flatten().copied().collect::<Vec<_>>(),
                )),
            );
        }
        detail.insert("floor_digest".into(), Bytes(lib::Blob(digest.to_vec())));
        detail.insert("floor_named_by".into(), Unsigned(named_by));
        detail.insert("floor_sequence".into(), Unsigned(sequence));
        detail.insert("observed_floor".into(), Text(observed.into()));
        if sequence >= 1 {
            at = Some(lib::TapeAt {
                tape,
                sequence,
                record_digest: digest,
            });
        }
    }
    if let Some((mark, observed)) = mark {
        detail.insert("mark_offset".into(), Unsigned(mark.offset));
        detail.insert("mark_sequence".into(), Unsigned(mark.sequence));
        detail.insert("observed_mark".into(), Text(observed.into()));
        if let Some((end, digest)) = mark.source {
            detail.insert("source_byte_end".into(), Unsigned(end));
            detail.insert("source_digest".into(), Bytes(lib::Blob(digest.to_vec())));
        }
    }
    let native = native.unwrap_or_default();
    detail.insert("observed_closed_end".into(), Unsigned(native.closed_end));
    detail.insert("observed_last_closed".into(), Unsigned(native.last_closed));
    lib::FenceRaised {
        kind: lib::FenceKind::Write,
        scope: lib::FenceScope::Tape(tape),
        cause: lib::FenceCause::NativeLogBehindProjection,
        evidence: None,
        at,
        operation: None,
        detail: lib::Bounded(
            detail
                .into_iter()
                .map(|(k, v)| lib::DiagnosticEntry(k, v))
                .collect(),
        ),
    }
}

/// Decide all raises from one authority fold and one frozen pre-destruction mark set.
pub(crate) fn detect(
    fold: &lib::LibraryFold,
    marks: &BTreeMap<lib::Id, MarkEvidence>,
    view: &NativeTapeView,
) -> Vec<lib::FenceRaised> {
    let mut raises = Vec::new();
    for (id, state) in &fold.tapes {
        if state.retired {
            continue;
        }
        let native = extent(view, *id);
        let reference = fold.contradictions.get(&lib::fold::Scope::Tape(*id));
        let floor = reference
            .and_then(|r| r.floor.map(|f| (f, &r.floor_digests)))
            .and_then(|(f, digests)| {
                let observed = observe_floor(f, digests, native, || {
                    view.lookup(&lib::TapeAt {
                        tape: state.tape,
                        sequence: f.0,
                        record_digest: f.1,
                    })
                })?;
                let carried = fold.active_tape_fences(id).any(|(_, raise)| {
                    raise.cause == lib::FenceCause::NativeLogBehindProjection
                        && carries_floor(raise, f)
                });
                (!carried).then_some((f, digests, observed))
            });
        let mark = marks.get(id).and_then(|mark| {
            let observed = observe_mark(mark, native, |end| {
                view.snapshot(*id)
                    .ok_or(())?
                    .projection_prefix_digest(end)
                    .map_err(|_| ())
            })?;
            (!mark_retained(fold, id, mark)).then_some((mark, observed))
        });
        if floor.is_some() || mark.is_some() {
            raises.push(raise(state.tape, native, floor, mark));
        }
    }
    raises
}

/// Protect exact SQL evidence independently of floors and unrelated fences.
pub(crate) fn check_rebuild(
    fold: &lib::LibraryFold,
    marks: &BTreeMap<lib::Id, MarkEvidence>,
    view: &NativeTapeView,
) -> Result<(), crate::StateError> {
    for (id, state) in &fold.tapes {
        if state.retired {
            continue;
        }
        let Some(mark) = marks.get(id) else { continue };
        let native = extent(view, *id);
        if observe_mark(mark, native, |end| {
            view.snapshot(*id)
                .ok_or(())?
                .projection_prefix_digest(end)
                .map_err(|_| ())
        })
        .is_some()
            && !mark_retained(fold, id, mark)
        {
            let native = native.unwrap_or_default();
            let end = mark
                .source
                .map_or_else(|| "-".into(), |(end, _)| end.to_string());
            return Err(crate::StateError::ConfigInvalid(format!(
                "RebuildWouldDropMarkEvidence: tape {} mark {}/{end} log {}/{}; start the daemon, which fences, or retire the tape",
                uuid::Uuid::from_bytes(*id), mark.sequence, native.last_closed, native.closed_end,
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lib::DiagnosticValue::{Bytes, Text, Unsigned};

    fn native(last_closed: u64, closed_end: u64) -> Option<NativeExtent> {
        Some(NativeExtent {
            last_closed,
            closed_end,
        })
    }
    fn mark() -> MarkEvidence {
        MarkEvidence {
            sequence: 8,
            offset: 70,
            source: Some((90, [8; 32])),
        }
    }
    fn present(digest: lib::Hash) -> Result<lib::TapeLookup, lib::FoldError> {
        Ok(lib::TapeLookup::Present {
            kind: crate::tape_log::Tag::Initialized,
            digest,
        })
    }
    fn keys(raise: &lib::FenceRaised) -> Vec<&str> {
        raise.detail.0.iter().map(|e| e.0.as_str()).collect()
    }
    /// Each floor word precedes the next, and lookup failures cannot satisfy it.
    #[test]
    fn floor_observation_order_and_read_errors() {
        let f = (8, [8; 32], 42);
        let one = BTreeSet::from([[8; 32]]);
        let conflict = BTreeSet::from([[8; 32], [9; 32]]);
        assert_eq!(
            observe_floor(f, &conflict, None, || panic!("absent")),
            Some("absent")
        );
        assert_eq!(
            observe_floor(f, &conflict, native(1, 1), || panic!("conflict")),
            Some("digest_conflict")
        );
        assert_eq!(
            observe_floor(f, &one, native(7, 100), || panic!("behind")),
            Some("behind")
        );
        for answer in [
            present([9; 32]),
            Ok(lib::TapeLookup::NotYet),
            Ok(lib::TapeLookup::Contradiction),
            Ok(lib::TapeLookup::EmptyPrefix { digest: [8; 32] }),
            Err(lib::FoldError::TapeUnavailable("fault".into())),
        ] {
            assert_eq!(
                observe_floor(f, &one, native(8, 100), || answer),
                Some("digest_mismatch")
            );
        }
        assert_eq!(
            observe_floor(f, &one, native(8, 100), || present([8; 32])),
            None
        );
        assert_eq!(
            observe_floor((0, [8; 32], 42), &one, native(0, 100), || Ok(
                lib::TapeLookup::EmptyPrefix { digest: [8; 32] }
            )),
            None
        );
        assert_eq!(
            observe_floor((0, [8; 32], 42), &one, native(0, 100), || present([8; 32])),
            Some("digest_mismatch")
        );
    }
    /// Marks evaluate sequence before bytes before digest; fixture marks have no byte check.
    #[test]
    fn mark_observation_order_and_read_errors() {
        let m = mark();
        assert_eq!(observe_mark(&m, None, |_| panic!("absent")), Some("absent"));
        assert_eq!(
            observe_mark(&m, native(7, 1), |_| panic!("behind")),
            Some("behind")
        );
        assert_eq!(
            observe_mark(&m, native(8, 89), |_| panic!("short")),
            Some("short_bytes")
        );
        assert_eq!(
            observe_mark(&m, native(8, 90), |_| Ok([9; 32])),
            Some("digest_mismatch")
        );
        assert_eq!(
            observe_mark(&m, native(8, 90), |_| Err(())),
            Some("digest_mismatch")
        );
        assert_eq!(
            observe_mark(&m, native(8, 100), |end| {
                assert_eq!(end, 90);
                Ok([8; 32])
            }),
            None
        );
        let m = MarkEvidence { source: None, ..m };
        assert_eq!(
            observe_mark(&m, None, |_| panic!("no source")),
            Some("absent")
        );
        assert_eq!(
            observe_mark(&m, native(7, 0), |_| panic!("no source")),
            Some("behind")
        );
        assert_eq!(
            observe_mark(&m, native(8, 0), |_| panic!("no source")),
            None
        );
    }
    /// Raises have exact ordered key sets; carry compares only the specified identity values.
    #[test]
    fn raise_shapes_zero_floor_conflicts_and_exact_carry() {
        let tape = lib::TapeRef {
            uuid: [1; 16],
            incarnation: 1,
        };
        let f = (8, [8; 32], 42);
        let digests = BTreeSet::from([[9; 32], [8; 32]]);
        let m = mark();
        let floor = raise(tape, None, Some((f, &digests, "absent")), None);
        assert_eq!(
            keys(&floor),
            [
                "basis",
                "floor_digest",
                "floor_named_by",
                "floor_sequence",
                "observed_closed_end",
                "observed_floor",
                "observed_last_closed"
            ]
        );
        assert_eq!(detail(&floor, "basis"), Some(Text("floor".into())));
        assert_eq!(detail(&floor, "floor_named_by"), Some(Unsigned(42)));
        assert_eq!(detail(&floor, "observed_last_closed"), Some(Unsigned(0)));
        assert_eq!(detail(&floor, "observed_closed_end"), Some(Unsigned(0)));
        assert_eq!(
            detail(&floor, "observed_floor"),
            Some(Text("absent".into()))
        );
        assert_eq!(
            floor.at,
            Some(lib::TapeAt {
                tape,
                sequence: 8,
                record_digest: [8; 32]
            })
        );
        assert!(carries_floor(&floor, (8, [8; 32], 99)));
        assert!(!carries_floor(&floor, (9, [8; 32], 42)));
        assert!(!carries_floor(&floor, (8, [9; 32], 42)));
        assert!(!carries_mark(&floor, &m));
        let zero = raise(
            tape,
            native(0, 10),
            Some(((0, [8; 32], 42), &digests, "digest_conflict")),
            None,
        );
        assert_eq!(zero.at, None);
        assert_eq!(
            detail(&zero, "floor_digest"),
            Some(Bytes(lib::Blob(vec![8u8; 32])))
        );
        assert_eq!(
            detail(&zero, "conflict_digests"),
            Some(Bytes(lib::Blob([vec![8u8; 32], vec![9; 32]].concat())))
        );
        let mark_only = raise(tape, native(7, 80), None, Some((&m, "behind")));
        assert_eq!(
            keys(&mark_only),
            [
                "basis",
                "mark_offset",
                "mark_sequence",
                "observed_closed_end",
                "observed_last_closed",
                "observed_mark",
                "source_byte_end",
                "source_digest"
            ]
        );
        assert_eq!(mark_only.at, None);
        assert_eq!(detail(&mark_only, "basis"), Some(Text("mark".into())));
        assert!(carries_mark(&mark_only, &m));
        for other in [
            MarkEvidence {
                sequence: 9,
                ..m.clone()
            },
            MarkEvidence {
                offset: 71,
                ..m.clone()
            },
            MarkEvidence {
                source: Some((91, [8; 32])),
                ..m.clone()
            },
            MarkEvidence {
                source: Some((90, [9; 32])),
                ..m.clone()
            },
            MarkEvidence {
                source: None,
                ..m.clone()
            },
        ] {
            assert!(!carries_mark(&mark_only, &other));
        }
        let both = raise(
            tape,
            native(7, 80),
            Some((f, &digests, "digest_conflict")),
            Some((&m, "behind")),
        );
        assert_eq!(
            keys(&both),
            [
                "basis",
                "conflict_digests",
                "floor_digest",
                "floor_named_by",
                "floor_sequence",
                "mark_offset",
                "mark_sequence",
                "observed_closed_end",
                "observed_floor",
                "observed_last_closed",
                "observed_mark",
                "source_byte_end",
                "source_digest"
            ]
        );
        assert_eq!(detail(&both, "basis"), Some(Text("both".into())));
        assert_eq!(detail(&both, "observed_closed_end"), Some(Unsigned(80)));
        assert_eq!(detail(&both, "observed_last_closed"), Some(Unsigned(7)));
        assert!(carries_floor(&both, f) && carries_mark(&both, &m));
        let without = MarkEvidence {
            source: None,
            ..m.clone()
        };
        let legacy = raise(tape, None, None, Some((&without, "absent")));
        assert_eq!(
            keys(&legacy),
            [
                "basis",
                "mark_offset",
                "mark_sequence",
                "observed_closed_end",
                "observed_last_closed",
                "observed_mark"
            ]
        );
        assert!(carries_mark(&legacy, &without));
        assert!(!carries_mark(&legacy, &m));
        for r in [floor, zero, mark_only, both, legacy] {
            assert_eq!(r.kind, lib::FenceKind::Write);
            assert_eq!(r.scope, lib::FenceScope::Tape(tape));
            assert_eq!(r.cause, lib::FenceCause::NativeLogBehindProjection);
            assert!(r.evidence.is_none() && r.operation.is_none());
            assert!(keys(&r)
                .windows(2)
                .all(|w| w[0].as_bytes() < w[1].as_bytes()));
        }
    }
    /// The SQL reader joins both directions, preserves u64 values and defaults only missing marks.
    #[test]
    fn projection_marks_include_source_only_and_mark_only_rows() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("index.sqlite");
        let index = crate::projector::open(&path).unwrap().write_handle();
        assert!(index.tape_projection_marks().unwrap().is_empty());
        let conn = rusqlite::Connection::open(&path).unwrap();
        for id in [1u8, 2] {
            conn.execute(
                "insert into tape_log_projection values(?1,1,?2,?3)",
                rusqlite::params![
                    vec![id; 16],
                    8u64.to_be_bytes().as_slice(),
                    u64::MAX.to_be_bytes().as_slice()
                ],
            )
            .unwrap();
        }
        for id in [2u8, 3] {
            conn.execute(
                "insert into tape_projection_sources values(?1,?2,?3,?4)",
                rusqlite::params![
                    vec![id; 16],
                    8u64.to_be_bytes().as_slice(),
                    90u64.to_be_bytes().as_slice(),
                    vec![8u8; 32]
                ],
            )
            .unwrap();
        }
        let marks = index.tape_projection_marks().unwrap();
        assert_eq!(marks.len(), 3);
        assert_eq!(
            marks[&[1; 16]],
            MarkEvidence {
                sequence: 8,
                offset: u64::MAX,
                source: None
            }
        );
        assert_eq!(
            marks[&[2; 16]],
            MarkEvidence {
                sequence: 8,
                offset: u64::MAX,
                source: Some((90, [8; 32]))
            }
        );
        assert_eq!(
            marks[&[3; 16]],
            MarkEvidence {
                sequence: 0,
                offset: 0,
                source: Some((90, [8; 32]))
            }
        );
    }
    /// Only active cause-14 evidence on this tape carries an exact obligation.
    #[test]
    fn detector_carries_floor_and_mark_independently() {
        use crate::{library_runtime::LibraryRuntime, StateLockGuard, StatePaths};
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let p = root.path();
        let paths = StatePaths {
            config_path: p.join("config"),
            state_dir: p.join("state"),
            audit_dir: p.join("library"),
            journal_dir: p.join("native"),
            sqlite_path: p.join("index.sqlite"),
            tape_cache_dir: p.join("cache"),
            calibration_dir: p.join("calibration"),
        };
        let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
        let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
        let tape = lib::TapeRef {
            uuid: [1; 16],
            incarnation: 1,
        };
        runtime
            .append(|a, v| {
                a.reprovision(
                    lib::Reprovision {
                        tape,
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
        let view = runtime.0.view.lock().unwrap().clone();
        let mut marks = BTreeMap::from([(tape.uuid, mark())]);
        let evaluate =
            |marks: &BTreeMap<_, _>| runtime.0.authority.with_fold(|f| detect(f, marks, &view));
        let append = |raised| {
            runtime
                .append(|a, v| {
                    a.fence_raised(raised, lib::AuditActor::System, lib::SourceLayer::Layer4, v)
                })
                .unwrap()
                .sequence
        };
        let mut ordinary = raise(
            tape,
            None,
            Some(((8, [8; 32], 2), &BTreeSet::from([[8; 32]]), "absent")),
            Some((&mark(), "absent")),
        );
        ordinary.cause = lib::FenceCause::TransferError;
        append(ordinary);
        let first = evaluate(&marks);
        assert_eq!(first.len(), 1);
        assert_eq!(detail(&first[0], "basis"), Some(Text("both".into())));
        let first_sequence = append(first[0].clone());
        assert!(evaluate(&marks).is_empty());
        runtime.0.authority.with_fold(|fold| {
            assert!(mark_retained(fold, &tape.uuid, &mark()));
            assert!(!mark_retained(fold, &[2; 16], &mark()));
        });
        marks.get_mut(&tape.uuid).unwrap().offset += 1;
        let second = evaluate(&marks);
        assert_eq!(second.len(), 1);
        assert_eq!(detail(&second[0], "basis"), Some(Text("mark".into())));
        assert!(second[0].at.is_none());
        append(second[0].clone());
        assert!(evaluate(&marks).is_empty());
        runtime
            .append(|a, v| {
                a.fence_released(
                    lib::FenceReleased {
                        fence: first_sequence,
                        evidence: "fixture release".into(),
                        forced: false,
                        detail: lib::Bounded(vec![]),
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
        let third = evaluate(&marks);
        assert_eq!(third.len(), 1);
        assert_eq!(detail(&third[0], "basis"), Some(Text("floor".into())));
        runtime.0.authority.with_fold(|fold| {
            assert!(!mark_retained(fold, &tape.uuid, &mark()));
            assert!(check_rebuild(fold, &marks, &view).is_ok());
            assert!(
                check_rebuild(fold, &BTreeMap::from([(tape.uuid, mark())]), &view)
                    .unwrap_err()
                    .to_string()
                    .contains("RebuildWouldDropMarkEvidence")
            );
        });
    }
}
