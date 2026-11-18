//! Ruling 31 production-open tests. Each fixture owns real native/library logs
//! and reopens StateHandle after changing bytes with every prior owner dropped.
use super::*;
use crate::authority::{TapeAuthorityView, TapeAuthorityWriter};
use crate::tape_log::{authority::AuthoritySnapshot, CommitCursor, SessionEndReason};
use lib::DiagnosticValue::{Bytes, Text, Unsigned};
use std::collections::BTreeSet;

const T: lib::Id = [0x51; 16];
const B: lib::Id = [0x52; 16];
const TIME: &str = "2026-11-11T00:00:00Z";
fn tape(id: lib::Id) -> lib::TapeRef {
    lib::TapeRef {
        uuid: id,
        incarnation: 1,
    }
}
fn value(raise: &lib::FenceRaised, key: &str) -> lib::DiagnosticValue {
    raise
        .detail
        .0
        .iter()
        .find(|e| e.0.as_str() == key)
        .unwrap()
        .1
        .clone()
}
struct Fixture {
    paths: StatePaths,
    config: crate::RemConfig,
    state: Option<crate::StateHandle>,
    _root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config_text = format!(
            r#"
[daemon]
state_dir = "{0}/state"
default_idle_timeout_seconds = 1800
[journal]
dir = "{0}/native"
require_trusted_volume = false
[audit]
dir = "{0}/library"
fsync = true
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
            root.path().display()
        );
        let config = crate::parse_config_toml(&config_text).unwrap();
        std::fs::write(root.path().join("config.toml"), config_text).unwrap();
        let paths = StatePaths::from_config(root.path().join("config.toml"), &config);
        let state = crate::StateHandle::open_with_config(paths.clone(), config.clone()).unwrap();
        Self {
            paths,
            config,
            state: Some(state),
            _root: root,
        }
    }
    fn rt(&self) -> LibraryRuntime {
        self.state.as_ref().unwrap().library_runtime()
    }
    fn close(&mut self) {
        drop(self.state.take());
    }
    fn reopen(&mut self) {
        self.close();
        self.state = Some(
            crate::StateHandle::open_with_config(self.paths.clone(), self.config.clone()).unwrap(),
        );
    }
    fn path(&self, id: lib::Id) -> std::path::PathBuf {
        self.paths
            .journal_dir
            .join("tapes")
            .join(format!("{}.remtape", uuid::Uuid::from_bytes(id).simple()))
    }
    fn sql_loss(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let path =
                std::path::PathBuf::from(format!("{}{suffix}", self.paths.sqlite_path.display()));
            if path.exists() {
                std::fs::remove_file(path).unwrap();
            }
        }
    }
    fn provision(&self, id: lib::Id) {
        self.rt()
            .append(|a, v| {
                a.reprovision(
                    lib::Reprovision {
                        tape: tape(id),
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
    /// A sequence-one backup and an independently variable two-record session tail.
    fn native(&self, id: lib::Id, long: bool) -> (Vec<u8>, AuthoritySnapshot) {
        let authority = crate::TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            id,
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
        let prefix = std::fs::read(self.path(id)).unwrap();
        let time = if long {
            format!("2026-11-11T00:00:00.{}Z", "1".repeat(9))
        } else {
            TIME.into()
        };
        owner
            .open_session(
                id,
                time.clone(),
                crate::tape_log::SessionDrive {
                    serial: "DRV-TEST".into(),
                    vendor: None,
                    product: None,
                    endpoint: crate::library_log::DriveKey::Standalone(
                        crate::library_log::Standalone {
                            drive_serial: "DRV-TEST".into(),
                        },
                    ),
                    drive_uuid: None,
                },
            )
            .unwrap();
        owner
            .end_session(
                id,
                time,
                SessionEndReason::CleanClose,
                None,
                std::iter::empty(),
            )
            .unwrap();
        let snapshot = owner.freeze().unwrap();
        self.rt().refresh_native(&snapshot).unwrap();
        self.rt().catch_up().unwrap();
        (prefix, snapshot)
    }
    fn tail_without_runtime(&self, id: lib::Id, session: lib::Id) -> AuthoritySnapshot {
        let authority =
            crate::TapeAuthority::open_fixture(self.paths.journal_dir.join("tapes"), id).unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        owner
            .open_session(
                session,
                TIME.into(),
                crate::tape_log::SessionDrive {
                    serial: "DRV-TEST".into(),
                    vendor: None,
                    product: None,
                    endpoint: crate::library_log::DriveKey::Standalone(
                        crate::library_log::Standalone {
                            drive_serial: "DRV-TEST".into(),
                        },
                    ),
                    drive_uuid: None,
                },
            )
            .unwrap();
        owner
            .end_session(
                session,
                TIME.into(),
                SessionEndReason::CleanClose,
                None,
                std::iter::empty(),
            )
            .unwrap();
        owner.freeze().unwrap()
    }
    fn prepared(&self) -> AuthoritySnapshot {
        let authority =
            crate::TapeAuthority::open_fixture(self.paths.journal_dir.join("tapes"), T).unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        owner
            .publish_report(&(&crate::checkpoint::tests::record(T)).into())
            .unwrap();
        let mut plan = crate::checkpoint::tests::finalization_intent(T);
        let manual = plan.manual.as_mut().unwrap();
        manual.assignment_generation = 1;
        manual.assigned_pool_id = None;
        manual.expected_pool_id = None;
        owner.begin_finalization(&(&plan).into()).unwrap();
        let snapshot = owner.freeze().unwrap();
        self.rt().refresh_native(&snapshot).unwrap();
        self.rt().catch_up().unwrap();
        snapshot
    }
    fn name(&self, kind: u8, at: lib::TapeAt) -> u64 {
        self.rt()
            .append(|a, v| {
                let actor = lib::AuditActor::System;
                let layer = lib::SourceLayer::Layer4;
                match kind {
                    0 => a.supersede_tail(
                        lib::SupersedeTail {
                            at,
                            operation: Some([7; 16]),
                            session_id: Some([8; 16]),
                            preview_digest: Some([0; 32]),
                            tail_file_count: 0,
                        },
                        actor,
                        layer,
                        v,
                    ),
                    1 => a.manual_finalization_accepted(
                        lib::ManualFinalizationAccepted {
                            at,
                            operation: [0x61; 16],
                            actor_fingerprint: "operator".into(),
                            idempotency_key: [5; 16],
                            request_fingerprint: [6; 32],
                            assignment_generation: 1,
                        },
                        actor,
                        layer,
                        v,
                    ),
                    2 => a.accept_unprotected_finalization(
                        lib::AcceptUnprotectedFinalization {
                            at,
                            operation: [7; 16],
                            w: 1,
                            t: 2,
                            reason: "fixture".into(),
                        },
                        actor,
                        layer,
                        v,
                    ),
                    3 => a.verification_progress(
                        lib::VerificationProgress {
                            at,
                            operation: [7; 16],
                            plan_digest: [3; 32],
                            modes: 1,
                            parts_done: 1,
                            next: lib::VerificationCursor {
                                partition: 0,
                                file: 1,
                                lba: 1,
                                ordinal: 1,
                                part: 1,
                            },
                            completed_digest: [4; 32],
                        },
                        actor,
                        layer,
                        v,
                    ),
                    4 => a.verification_verdict(
                        lib::VerificationVerdict {
                            tape: at.tape,
                            at: Some(at),
                            outcome: lib::VerificationOutcome::VerifiedComplete,
                            operation: [7; 16],
                            damaged: lib::Bounded(vec![]),
                            unprotected: None,
                            modes: 1,
                            edition_digest: None,
                            progress: None,
                        },
                        actor,
                        layer,
                        v,
                    ),
                    5 => a.recovery_recorded(
                        lib::RecoveryRecorded {
                            operation: [7; 16],
                            prior: None,
                            action: lib::RecoveryAction::RepairApplied(lib::RepairApplied {
                                retained_through: at.sequence,
                                archived_prefix_digest: [42; 32],
                            }),
                            at,
                        },
                        actor,
                        layer,
                        v,
                    ),
                    _ => a.fence_raised(
                        lib::FenceRaised {
                            kind: lib::FenceKind::Write,
                            scope: lib::FenceScope::Tape(at.tape),
                            cause: lib::FenceCause::TransferError,
                            evidence: None,
                            at: Some(at),
                            operation: None,
                            detail: lib::Bounded(vec![]),
                        },
                        actor,
                        layer,
                        v,
                    ),
                }
            })
            .unwrap()
            .sequence
    }
    fn raises(&self, id: lib::Id) -> Vec<(u64, lib::FenceRaised)> {
        self.rt().0.authority.with_fold(|fold| {
            fold.active_tape_fences(&id)
                .filter(|(_, r)| r.cause == lib::FenceCause::NativeLogBehindProjection)
                .map(|(n, r)| (n, r.clone()))
                .collect()
        })
    }
    fn retire(&mut self, id: lib::Id) {
        self.state
            .as_mut()
            .unwrap()
            .retire_tape(crate::RetireTapeInput {
                tape_uuid: id,
                reason: "fixture".into(),
            })
            .unwrap();
    }
}

/// Every received kind, including a released fence, survives SQL/log loss and refuses ordinary release.
#[test]
fn l1a_absent_floor_all_kinds_restart_and_release_refusal() {
    for kind in 0..8 {
        let mut f = Fixture::new();
        f.provision(T);
        let snapshot = f.prepared();
        let at = f.rt().native_reference(&snapshot).unwrap();
        let named_by = f.name(kind, at.clone());
        if kind == 7 {
            f.rt()
                .append(|a, v| {
                    a.fence_released(
                        lib::FenceReleased {
                            fence: named_by,
                            evidence: "fixture".into(),
                            forced: false,
                            detail: lib::Bounded(vec![]),
                        },
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer4,
                        v,
                    )
                })
                .unwrap();
        }
        f.close();
        f.sql_loss();
        std::fs::rename(f.path(T), f._root.path().join("saved.remtape")).unwrap();
        f.reopen();
        for _ in 0..2 {
            let raises = f.raises(T);
            assert_eq!(raises.len(), 1, "kind {kind}");
            let (sequence, raised) = &raises[0];
            assert_eq!(
                raised,
                &lib::FenceRaised {
                    kind: lib::FenceKind::Write,
                    scope: lib::FenceScope::Tape(tape(T)),
                    cause: lib::FenceCause::NativeLogBehindProjection,
                    evidence: None,
                    at: Some(at.clone()),
                    operation: None,
                    detail: lib::Bounded(vec![
                        lib::DiagnosticEntry("basis".into(), Text("floor".into())),
                        lib::DiagnosticEntry(
                            "floor_digest".into(),
                            Bytes(lib::Blob(at.record_digest.to_vec()))
                        ),
                        lib::DiagnosticEntry("floor_named_by".into(), Unsigned(named_by)),
                        lib::DiagnosticEntry("floor_sequence".into(), Unsigned(at.sequence)),
                        lib::DiagnosticEntry("observed_closed_end".into(), Unsigned(0)),
                        lib::DiagnosticEntry("observed_floor".into(), Text("absent".into())),
                        lib::DiagnosticEntry("observed_last_closed".into(), Unsigned(0)),
                    ]),
                }
            );
            let before = f.rt().0.authority.with_fold(|fold| fold.received_through);
            let error = f
                .rt()
                .release_tape_fence(
                    &format!("library:{sequence}"),
                    "acknowledged",
                    false,
                    lib::AuditActor::System,
                )
                .unwrap_err();
            let RuntimeError::State(StateError::ConfigInvalid(message)) = error else {
                panic!("wrong release refusal")
            };
            assert_eq!(message,"native_log_behind_projection fence requires --force: restore the exact tape log, then release with --force; or rebuild the catalog with rem rebuild-catalog-from-journals, then release with --force; or retire the tape");
            assert_eq!(
                f.rt().0.authority.with_fold(|fold| fold.received_through),
                before
            );
            f.reopen();
        }
    }
}

/// Fresh/retired identities have no obligation, while a published zero prefix remains a floor.
#[test]
fn l1a_fresh_retired_and_zero_floor() {
    let mut f = Fixture::new();
    f.provision(T);
    f.close();
    f.sql_loss();
    f.reopen();
    assert!(f.raises(T).is_empty());
    let (_, snapshot) = f.native(T, false);
    let at = f.rt().native_reference(&snapshot).unwrap();
    f.name(0, at);
    f.retire(T);
    f.close();
    f.sql_loss();
    std::fs::remove_file(f.path(T)).unwrap();
    f.reopen();
    assert!(f.raises(T).is_empty());
    f.provision(B);
    let authority =
        crate::TapeAuthority::open(f.paths.journal_dir.join("tapes"), B, &f.paths.sqlite_path)
            .unwrap();
    let mut owner = authority.acquire_exclusive().unwrap();
    owner.publish_empty_prefix(262144, None).unwrap();
    let snapshot = owner.freeze().unwrap();
    let at = f.rt().native_reference(&snapshot).unwrap();
    assert_eq!(at.sequence, 0);
    f.rt().refresh_native(&snapshot).unwrap();
    f.name(0, at.clone());
    drop(owner);
    drop(authority);
    f.close();
    f.sql_loss();
    std::fs::remove_file(f.path(B)).unwrap();
    f.reopen();
    let raises = f.raises(B);
    assert_eq!(raises.len(), 1);
    let raised = &raises[0].1;
    assert!(raised.at.is_none());
    assert_eq!(value(raised, "floor_sequence"), Unsigned(0));
    assert_eq!(
        value(raised, "floor_digest"),
        Bytes(lib::Blob(at.record_digest.to_vec()))
    );
    f.reopen();
    assert_eq!(f.raises(B), raises);
}

/// Only cause 14 suppresses projection; every other admitted tape cause must catch up.
#[test]
fn l1a_other_tape_fences_do_not_skip_projection() {
    let mut f = Fixture::new();
    f.provision(T);
    let (_, before) = f.native(T, false);
    for cause in [
        lib::FenceCause::PartialBatch,
        lib::FenceCause::TransferError,
        lib::FenceCause::CloseoutCheckMismatch,
        lib::FenceCause::SupersedeTailFailed,
        lib::FenceCause::StandaloneWriteLostByRestart,
        lib::FenceCause::AppendAdmissionRefused,
        lib::FenceCause::ResetUnitAttention,
        lib::FenceCause::PositionDrift,
        lib::FenceCause::AuthorityAmbiguous,
        lib::FenceCause::SessionSuffixContradiction,
        lib::FenceCause::CloseoutReadContradiction,
        lib::FenceCause::Cleaning,
        lib::FenceCause::CheckpointBarrierFailed,
    ] {
        f.rt()
            .append(|a, v| {
                a.fence_raised(
                    lib::FenceRaised {
                        kind: lib::FenceKind::Write,
                        scope: lib::FenceScope::Tape(tape(T)),
                        cause,
                        evidence: None,
                        at: None,
                        operation: None,
                        detail: lib::Bounded(vec![]),
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
    }
    f.close();
    let tail = f.tail_without_runtime(T, B);
    assert!(tail.log_reference().0 > before.log_reference().0);
    f.reopen();
    assert_eq!(
        f.rt()
            .reader()
            .unwrap()
            .projected_through_sequence(T)
            .unwrap(),
        tail.log_reference().0
    );
    assert!(f.raises(T).is_empty());
}

/// Both obligations are evaluated even when crash-ack validation has nothing to replay.
#[test]
fn l1a_short_log_basis_skip_and_other_tape_projection() {
    for (sql, floor, basis) in [
        (false, true, "floor"),
        (true, false, "mark"),
        (true, true, "both"),
    ] {
        let mut f = Fixture::new();
        f.provision(T);
        f.provision(B);
        let (prefix, snapshot) = f.native(T, false);
        f.native(B, false);
        let at = f.rt().native_reference(&snapshot).unwrap();
        let named_by = floor.then(|| f.name(0, at.clone()));
        let old = f.rt().reader().unwrap().tape_projection_marks().unwrap()[&T].clone();
        f.close();
        std::fs::write(f.path(T), prefix).unwrap();
        let healthy = f.tail_without_runtime(B, [0x99; 16]);
        if !sql {
            f.sql_loss();
        }
        f.reopen();
        assert_eq!(
            f.rt().take_projection_measurement().1.len(),
            1,
            "only the healthy tape entered project(Tape)"
        );
        let raises = f.raises(T);
        assert_eq!(raises.len(), 1);
        let raised = &raises[0].1;
        let keys: Vec<_> = raised
            .detail
            .0
            .iter()
            .map(|entry| entry.0.as_str())
            .collect();
        let expected = match basis {
            "floor" => vec![
                "basis",
                "floor_digest",
                "floor_named_by",
                "floor_sequence",
                "observed_closed_end",
                "observed_floor",
                "observed_last_closed",
            ],
            "mark" => vec![
                "basis",
                "mark_offset",
                "mark_sequence",
                "observed_closed_end",
                "observed_last_closed",
                "observed_mark",
                "source_byte_end",
                "source_digest",
            ],
            "both" => vec![
                "basis",
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
                "source_digest",
            ],
            _ => unreachable!(),
        };
        assert_eq!(keys, expected, "whole detail key list for {basis}");
        assert_eq!(value(raised, "basis"), Text(basis.into()));
        if floor {
            assert_eq!(value(raised, "floor_sequence"), Unsigned(at.sequence));
            assert_eq!(
                value(raised, "floor_digest"),
                Bytes(lib::Blob(at.record_digest.to_vec()))
            );
            assert_eq!(value(raised, "floor_named_by"), Unsigned(named_by.unwrap()));
            assert_eq!(value(raised, "observed_floor"), Text("behind".into()));
            assert_eq!(raised.at, Some(at));
            assert!(
                f.rt()
                    .0
                    .authority
                    .with_fold(|fold| fold.contradictions[&lib::fold::Scope::Tape(T)].excluded)
                    >= 1
            );
        } else {
            assert!(raised.at.is_none());
        }
        if sql {
            assert_eq!(value(raised, "observed_mark"), Text("behind".into()));
            assert_eq!(value(raised, "mark_sequence"), Unsigned(old.sequence));
            assert_eq!(value(raised, "mark_offset"), Unsigned(old.offset));
            let (end, digest) = old.source.unwrap();
            assert_eq!(value(raised, "source_byte_end"), Unsigned(end));
            assert_eq!(
                value(raised, "source_digest"),
                Bytes(lib::Blob(digest.to_vec()))
            );
            assert_eq!(
                f.rt().reader().unwrap().tape_projection_marks().unwrap()[&T],
                old
            );
            assert_eq!(
                f.rt()
                    .reader()
                    .unwrap()
                    .session_state(uuid::Uuid::from_bytes(T))
                    .unwrap()
                    .as_deref(),
                Some("closed")
            );
        }
        assert_eq!(
            f.rt()
                .reader()
                .unwrap()
                .projected_through_sequence(B)
                .unwrap(),
            healthy.log_reference().0
        );
        f.rt().fixture_mark_projection_dirty();
        f.rt().catch_up().unwrap();
        assert_eq!(f.rt().take_projection_measurement().1.len(), 1);
        f.rt().catch_up().unwrap();
        assert_eq!(f.rt().take_projection_measurement().1.len(), 0);
        f.reopen();
        assert_eq!(f.raises(T), raises);
    }
}

/// Equal sequence counts do not hide rewritten bytes or a shorter closing record.
#[test]
fn l1a_digest_rewrite_and_short_bytes_skip_projection() {
    for short in [false, true] {
        for floor in [false, true] {
            let mut f = Fixture::new();
            f.provision(T);
            let (prefix, old_snapshot) = f.native(T, short);
            let at = f.rt().native_reference(&old_snapshot).unwrap();
            if floor {
                f.name(0, at);
            }
            let old = f.rt().reader().unwrap().tape_projection_marks().unwrap()[&T].clone();
            f.close();
            std::fs::write(f.path(T), prefix).unwrap();
            let replacement = f.tail_without_runtime(T, [0x91; 16]);
            assert_eq!(
                replacement.log_reference().0,
                old_snapshot.log_reference().0
            );
            assert_eq!(
                replacement.raw.summary().closed_end < old.source.unwrap().0,
                short
            );
            f.reopen();
            assert_eq!(
                f.rt().take_projection_measurement().1.len(),
                0,
                "fenced tape never entered project(Tape)"
            );
            let raises = f.raises(T);
            assert_eq!(raises.len(), 1);
            let raised = &raises[0].1;
            assert_eq!(
                value(raised, "observed_mark"),
                Text(
                    if short {
                        "short_bytes"
                    } else {
                        "digest_mismatch"
                    }
                    .into()
                )
            );
            assert_eq!(
                value(raised, "basis"),
                Text(if floor { "both" } else { "mark" }.into())
            );
            if floor {
                assert_eq!(
                    value(raised, "observed_floor"),
                    Text("digest_mismatch".into())
                );
            }
            assert_eq!(
                f.rt().reader().unwrap().tape_projection_marks().unwrap()[&T],
                old
            );
            f.rt().fixture_mark_projection_dirty();
            f.rt().catch_up().unwrap();
            assert_eq!(f.rt().take_projection_measurement().1.len(), 0);
            f.rt().catch_up().unwrap();
            assert_eq!(f.rt().take_projection_measurement().1.len(), 0);
            f.reopen();
            assert_eq!(f.raises(T), raises);
        }
    }
}

/// Conflicting fence-only evidence is carried in sorted order even while the source exists.
#[test]
fn l1a_floor_conflict_at_open() {
    let mut f = Fixture::new();
    f.provision(T);
    let (_, snapshot) = f.native(T, false);
    let at = f.rt().native_reference(&snapshot).unwrap();
    let named_by = f.name(6, at.clone());
    let mut other = at.clone();
    other.record_digest = [0x19; 32];
    f.name(6, other.clone());
    f.close();
    f.sql_loss();
    f.reopen();
    let raises = f.raises(T);
    assert_eq!(raises.len(), 1);
    let raised = &raises[0].1;
    assert_eq!(value(raised, "basis"), Text("floor".into()));
    assert_eq!(
        value(raised, "observed_floor"),
        Text("digest_conflict".into())
    );
    assert_eq!(value(raised, "floor_named_by"), Unsigned(named_by));
    assert_eq!(raised.at, Some(at.clone()));
    let digests = BTreeSet::from([at.record_digest, other.record_digest]);
    assert_eq!(
        value(raised, "conflict_digests"),
        Bytes(lib::Blob(digests.iter().flatten().copied().collect()))
    );
    f.reopen();
    assert_eq!(f.raises(T), raises);
}

/// The skip is limited to active cause-14 tapes; a retired unexamined short source still errors.
#[test]
fn l1a_retired_short_source_still_aborts_projection() {
    for retired in [false, true] {
        let mut f = Fixture::new();
        f.provision(T);
        let (prefix, _) = f.native(T, false);
        if retired {
            f.retire(T);
        }
        f.close();
        std::fs::write(f.path(T), prefix).unwrap();
        let result = crate::StateHandle::open_with_config(f.paths.clone(), f.config.clone());
        if retired {
            assert!(result.is_err());
            assert!(result
                .err()
                .unwrap()
                .to_string()
                .contains("projection source is shorter than its digest"));
        } else {
            f.state = Some(result.unwrap());
            assert_eq!(f.raises(T).len(), 1);
        }
    }
}

/// There is no crash acknowledgement to make validate replay the floor.
#[test]
fn l1a_rebuild_detects_floor_from_explicit_replay() {
    let mut f = Fixture::new();
    f.provision(T);
    let (prefix, snapshot) = f.native(T, false);
    let at = f.rt().native_reference(&snapshot).unwrap();
    f.close();
    std::fs::write(f.path(T), prefix).unwrap();
    f.sql_loss();
    f.reopen();
    assert!(f.raises(T).is_empty());
    // A fence-only reference is admissible even though the native prefix is short.
    let named_by = f.name(6, at.clone());
    f.state
        .as_mut()
        .unwrap()
        .rebuild_projection_from_tape_logs()
        .unwrap();
    let raises = f.raises(T);
    assert_eq!(raises.len(), 1);
    assert_eq!(raises[0].1.at, Some(at));
    assert_eq!(value(&raises[0].1, "basis"), Text("floor".into()));
    assert_eq!(value(&raises[0].1, "floor_named_by"), Unsigned(named_by));
    assert_eq!(value(&raises[0].1, "observed_floor"), Text("behind".into()));
    assert_eq!(
        f.rt().reader().unwrap().tape_projection_marks().unwrap()[&T].sequence,
        1
    );
    f.state
        .as_mut()
        .unwrap()
        .rebuild_projection_from_tape_logs()
        .unwrap();
    assert_eq!(f.raises(T), raises);
    f.close();
    f.sql_loss();
    f.reopen();
    assert_eq!(f.raises(T), raises);
}

#[test]
fn l1a_offline_preserves_exact_mark_above_carried_floor() {
    let mut f = Fixture::new();
    f.provision(T);
    let (prefix, snapshot) = f.native(T, false);
    let at = f.rt().native_reference(&snapshot).unwrap();
    let through_floor = std::fs::read(f.path(T)).unwrap();
    f.name(0, at);
    let snapshot = f.tail_without_runtime(T, B);
    f.rt().refresh_native(&snapshot).unwrap();
    f.rt().catch_up().unwrap();
    let mark = f.rt().reader().unwrap().tape_projection_marks().unwrap()[&T].clone();
    assert_eq!(mark.sequence, 5);
    f.close();
    let sql = std::fs::read(&f.paths.sqlite_path).unwrap();
    std::fs::write(f.path(T), &prefix).unwrap();
    f.sql_loss();
    f.reopen();
    let floor_raises = f.raises(T);
    assert_eq!(floor_raises.len(), 1);
    assert_eq!(value(&floor_raises[0].1, "basis"), Text("floor".into()));
    f.close();
    f.sql_loss();
    f.reopen();
    assert_eq!(f.raises(T), floor_raises);
    f.close();
    f.sql_loss();
    std::fs::write(&f.paths.sqlite_path, sql).unwrap();
    let library_files = || {
        std::fs::read_dir(&f.paths.audit_dir)
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let library_before = library_files();
    for (bytes, sequence) in [(&prefix, 1), (&through_floor, 3)] {
        std::fs::write(f.path(T), bytes).unwrap();
        let error = crate::StateHandle::rebuild_projection_from_config_file(&f.paths.config_path)
            .unwrap_err();
        let crate::StateError::ConfigInvalid(message) = error else {
            panic!("{error}")
        };
        assert_eq!(message, format!(
            "RebuildWouldDropMarkEvidence: tape {} mark 5/{} log {sequence}/{}; start the daemon, which fences, or retire the tape",
            uuid::Uuid::from_bytes(T), mark.source.unwrap().0, bytes.len(),
        ));
        assert_eq!(std::fs::read(f.path(T)).unwrap(), *bytes);
        assert_eq!(library_files(), library_before);
        let catalog = crate::projector::open(&f.paths.sqlite_path).unwrap();
        assert_eq!(
            catalog.write_handle().tape_projection_marks().unwrap()[&T],
            mark
        );
    }
    f.reopen();
    let raises = f.raises(T);
    assert_eq!(raises.len(), 2);
    assert_eq!(raises[0], floor_raises[0]);
    assert_eq!(value(&raises[1].1, "basis"), Text("mark".into()));
    assert_eq!(value(&raises[1].1, "mark_sequence"), Unsigned(5));
    assert_eq!(value(&raises[1].1, "mark_offset"), Unsigned(mark.offset));
    assert_eq!(
        value(&raises[1].1, "source_byte_end"),
        Unsigned(mark.source.unwrap().0)
    );
    assert_eq!(
        value(&raises[1].1, "source_digest"),
        Bytes(lib::Blob(mark.source.unwrap().1.to_vec()))
    );
    f.close();
    crate::StateHandle::rebuild_projection_from_config_file(&f.paths.config_path).unwrap();
    f.reopen();
    assert_eq!(f.raises(T), raises);
    assert_eq!(
        f.rt().reader().unwrap().tape_projection_marks().unwrap()[&T].sequence,
        3
    );
    f.close();
    f.sql_loss();
    std::fs::write(f.path(T), prefix).unwrap();
    crate::StateHandle::rebuild_projection_from_config_file(&f.paths.config_path).unwrap();
    f.reopen();
    assert_eq!(f.raises(T), raises);
}

/// Frozen identity plus length detects replacement, truncation and disappearance.
#[test]
fn l1b_prefix_source_freshness_real_files() {
    use lib::native_view::NativePrefixDigests;
    use native_floor::PrefixDigestSource;
    for change in ["unchanged", "replace", "truncate", "remove"] {
        let f = Fixture::new();
        f.provision(T);
        let (short, _) = f.native(T, false);
        let runtime = f.rt();
        let view = runtime.0.view.lock().unwrap().clone();
        let source = NativePrefixDigests {
            view: &view,
            root: runtime.0.manifest.tape_root(),
        };
        assert!(source.source_unchanged(T).unwrap());
        match change {
            "replace" => {
                let old = f.path(T).with_extension("old");
                std::fs::rename(f.path(T), &old).unwrap();
                std::fs::copy(old, f.path(T)).unwrap();
            }
            "truncate" => std::fs::OpenOptions::new()
                .write(true)
                .open(f.path(T))
                .unwrap()
                .set_len(short.len() as u64)
                .unwrap(),
            "remove" => std::fs::remove_file(f.path(T)).unwrap(),
            _ => {}
        }
        assert_eq!(
            matches!(source.source_unchanged(T), Ok(true)),
            change == "unchanged",
            "{change}"
        );
        assert!(source.source_unchanged(B).is_err());
        assert!(source.prefix_digest(B, 0).is_err());
    }
}

/// The waiver compares every recorded value and never suppresses an independent mark.
#[test]
fn l1b_detector_exact_waiver_and_independent_mark() {
    use lib::native_view::NativePrefixDigests;
    let f = Fixture::new();
    f.provision(T);
    f.native(T, false);
    let runtime = f.rt();
    let view = runtime.0.view.lock().unwrap().clone();
    let source = NativePrefixDigests {
        view: &view,
        root: runtime.0.manifest.tape_root(),
    };
    let mut fold = runtime.0.authority.with_fold(Clone::clone);
    let extent = native_floor::extent(&view, T).unwrap();
    fold.contradictions.insert(
        lib::fold::Scope::Tape(T),
        lib::fold::TapeReferences {
            floor: Some((extent.last_closed + 1, [8; 32], 42)),
            floor_digests: BTreeSet::from([[8; 32]]),
            ..Default::default()
        },
    );
    let memory =
        native_floor::force_release_decision(&fold, tape(T), Some(extent), None, &view, &source)
            .unwrap();
    fold.forced_observation.insert(T, memory.clone());
    let empty = std::collections::BTreeMap::new();
    assert!(native_floor::detect(&fold, &empty, &view, &source).is_empty());
    for i in 0..7 {
        let mut altered = memory.clone();
        altered.0[i].1 = Text("changed".into());
        fold.forced_observation.insert(T, altered);
        let raised = native_floor::detect(&fold, &empty, &view, &source);
        assert_eq!(raised.len(), 1, "key {i}");
        assert_eq!(value(&raised[0], "basis"), Text("floor".into()));
    }
    fold.forced_observation.insert(T, memory);
    let marks = std::collections::BTreeMap::from([(
        T,
        native_floor::MarkEvidence {
            sequence: extent.last_closed + 1,
            offset: 0,
            source: None,
        },
    )]);
    let raised = native_floor::detect(&fold, &marks, &view, &source);
    assert_eq!(raised.len(), 1);
    assert_eq!(value(&raised[0], "basis"), Text("mark".into()));
}

/// Refusal counts are observed through the durable library records, not SQL text.
fn l1b_refusals(f: &Fixture) -> Vec<(u64, String, String, Option<String>)> {
    f.rt()
        .test_source_records()
        .into_iter()
        .filter_map(|record| {
            let lib::BodyView::Diagnostic(d) = record.body_view() else {
                return None;
            };
            if d.event != lib::DiagnosticEvent::RefusalRecorded {
                return None;
            }
            let (Unsigned(code), Text(name)) = (&d.detail.0[0].1, &d.detail.0[1].1) else {
                panic!("refusal detail")
            };
            Some((
                *code,
                name.as_str().into(),
                d.subject_kind.as_str().into(),
                d.subject_id.as_ref().map(|v| v.as_str().into()),
            ))
        })
        .collect()
}
fn l1b_force(
    f: &mut Fixture,
    sequence: u64,
) -> Result<Option<crate::TapeIoFenceRecord>, StateError> {
    f.state.as_mut().unwrap().release_tape_io_fence(
        &format!("library:{sequence}"),
        "operator checked evidence",
        true,
    )
}
fn l1b_release_record(f: &Fixture) -> lib::FenceReleased {
    f.rt()
        .test_source_records()
        .into_iter()
        .rev()
        .find_map(|r| match r.body_view() {
            lib::BodyView::FenceReleased(v) => Some(v.clone()),
            _ => None,
        })
        .unwrap()
}
fn l1b_assert_refused_once(f: &mut Fixture, sequence: u64, code: u64, reason: &str) {
    let before = l1b_refusals(f);
    let error = l1b_force(f, sequence).unwrap_err().to_string();
    assert!(error.contains(reason), "{error}");
    let after = l1b_refusals(f);
    assert_eq!(after.len(), before.len() + 1);
    assert_eq!(
        after.last().unwrap(),
        &(
            code,
            if code == 2 {
                "ContradictedTapeAdmission"
            } else {
                "TapeLogBehindProjection"
            }
            .into(),
            "tape".into(),
            Some(uuid::Uuid::from_bytes(T).to_string())
        )
    );
    assert!(f.rt().active_fence(sequence).is_some());
}

/// A mark can only be released after exact restoration or rebuild; a later loss
/// creates a fresh fence even when the last forced release remembered nothing.
#[test]
fn l1b_release_mark_restore_rebuild_and_lost_twice() {
    for rebuild in [false, true] {
        let mut f = Fixture::new();
        f.provision(T);
        let (short, snapshot) = f.native(T, false);
        let at = f.rt().native_reference(&snapshot).unwrap();
        let full = std::fs::read(f.path(T)).unwrap();
        f.close();
        std::fs::write(f.path(T), &short).unwrap();
        let offline = crate::StateHandle::rebuild_projection_from_config_file(&f.paths.config_path)
            .unwrap_err();
        assert!(offline.to_string().contains("RebuildWouldDropMarkEvidence"));
        f.reopen();
        let sequence = f.raises(T)[0].0;
        l1b_assert_refused_once(&mut f, sequence, 1, "live projection mark is above");
        f.close();
        if rebuild {
            crate::StateHandle::rebuild_projection_from_config_file(&f.paths.config_path).unwrap();
        } else {
            std::fs::write(f.path(T), &full).unwrap();
        }
        f.reopen();
        assert_eq!(
            l1b_force(&mut f, sequence).unwrap().unwrap().state,
            "released"
        );
        if !rebuild {
            f.rt().take_projection_measurement();
            f.rt().catch_up().unwrap();
            assert_eq!(
                f.rt().take_projection_measurement().1.len(),
                1,
                "released tape gets one Apply"
            );
        }
        let release = l1b_release_record(&f);
        assert!(release.forced && release.detail.0.is_empty());
        f.reopen();
        assert!(f.raises(T).is_empty());
        if rebuild {
            f.name(6, at);
            f.reopen();
            assert_eq!(f.raises(T).len(), 1);
        } else {
            f.close();
            std::fs::write(f.path(T), &short).unwrap();
            f.reopen();
            assert_eq!(f.raises(T).len(), 1);
            assert_ne!(f.raises(T)[0].0, sequence);
            f.close();
            std::fs::remove_file(f.path(T)).unwrap();
            f.reopen();
            assert_eq!(f.raises(T).len(), 1);
            let sequence = f.raises(T)[0].0;
            l1b_assert_refused_once(&mut f, sequence, 1, "tape log on disk changed");
        }
    }
}

/// Force refuses contradictions, conflicting floors and both byte/digest mark
/// losses, while ordinary acknowledgements keep forced=false even with force set.
#[test]
fn l1b_release_refusal_codes_and_other_causes() {
    for fault in [
        "contradicted",
        "floor_conflict",
        "short_bytes",
        "source_digest",
    ] {
        let mut f = Fixture::new();
        f.provision(T);
        let (short, snapshot) = f.native(T, fault == "short_bytes");
        let at = f.rt().native_reference(&snapshot).unwrap();
        if fault == "contradicted" {
            f.name(4, at.clone());
        }
        if fault == "floor_conflict" {
            f.name(6, at.clone());
            f.name(
                6,
                lib::TapeAt {
                    record_digest: [0x19; 32],
                    ..at
                },
            );
        }
        f.close();
        if fault == "contradicted" {
            f.sql_loss();
            std::fs::write(f.path(T), &short).unwrap();
        } else if fault != "floor_conflict" {
            std::fs::write(f.path(T), &short).unwrap();
            f.tail_without_runtime(T, [0x91; 16]);
        }
        f.reopen();
        let sequence = f.raises(T)[0].0;
        let (code, reason) = match fault {
            "contradicted" => (2, "ContradictedTapeAdmission"),
            "floor_conflict" => (1, "floor digests conflict"),
            "short_bytes" => (1, "source bytes extend beyond"),
            _ => (1, "source digest differs"),
        };
        l1b_assert_refused_once(&mut f, sequence, code, reason);
    }
    let mut f = Fixture::new();
    f.provision(T);
    let (_, snapshot) = f.native(T, false);
    let sequence = f.name(6, f.rt().native_reference(&snapshot).unwrap());
    l1b_force(&mut f, sequence).unwrap();
    assert!(!l1b_release_record(&f).forced);
    assert!(l1b_refusals(&f).is_empty());
}

/// Force preserves the exact scope/suffix errors without appending or releasing anything.
#[test]
fn l1b_force_preserves_scope_and_suffix_refusals() {
    for (scope, cause, expected) in [
        (
            lib::FenceScope::Drive(lib::DriveKey::Bay(lib::Bay {
                library_serial: "LIBA0001".into(),
                bay: 1,
            })),
            lib::FenceCause::TransferError,
            "fence is not tape-scoped",
        ),
        (
            lib::FenceScope::Tape(tape(T)),
            lib::FenceCause::SessionSuffixContradiction,
            "release refused: a session_suffix_contradiction fence stays until rem tape supersede-tail recovers the tape's tail (it refuses when the tape contradicts its recorded session ending); otherwise withdraw the tape with rem tape retire or start from a newer copy of the state directory",
        ),
        (
            lib::FenceScope::Tape(tape(T)),
            lib::FenceCause::AppendAdmissionRefused,
            "release refused: an append_admission_refused fence keeps the tape closed for writing, and readable, until rem tape supersede-tail recovers its tail (it refuses when this state shows the tape's last session closed); otherwise withdraw the tape with rem tape retire or start from a newer copy of the state directory",
        ),
    ] {
        let f = Fixture::new();
        f.provision(T);
        let runtime = f.rt();
        let raised = lib::FenceRaised {
            kind: if matches!(scope, lib::FenceScope::Drive(_)) {
                lib::FenceKind::DriveFault
            } else {
                lib::FenceKind::Write
            },
            scope,
            cause,
            evidence: None,
            at: None,
            operation: None,
            detail: lib::Bounded(vec![]),
        };
        let sequence = runtime
            .append(|a, v| {
                a.fence_raised(
                    raised.clone(),
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap()
            .sequence;
        let before = runtime.test_source_records().len();
        let error = runtime
            .release_tape_fence(
                &format!("library:{sequence}"),
                "operator checked evidence",
                true,
                lib::AuditActor::System,
            )
            .unwrap_err();
        match error {
            RuntimeError::State(StateError::ConfigInvalid(message)) => {
                assert_eq!(message, expected)
            }
            other => panic!("unexpected refusal: {other}"),
        }
        assert!(l1b_refusals(&f).is_empty());
        assert_eq!(
            runtime.test_source_records().len(),
            before,
            "force must not append for {expected}"
        );
        assert_eq!(runtime.0.authority.active_fence(sequence), Some(raised));
    }
}

/// Floor waivers survive only the exact bytes/extent, and an exact restoration
/// followed by an empty release clears the latest remembered observation.
#[test]
fn l1b_release_floor_memory_content_extent_and_clear() {
    for mismatch in [false, true] {
        let mut f = Fixture::new();
        f.provision(T);
        let (short, snapshot) = f.native(T, false);
        let full = std::fs::read(f.path(T)).unwrap();
        let at = f.rt().native_reference(&snapshot).unwrap();
        f.name(6, at);
        f.close();
        f.sql_loss();
        std::fs::write(f.path(T), &short).unwrap();
        if mismatch {
            f.tail_without_runtime(T, [0x91; 16]);
        }
        f.reopen();
        let seq = f.raises(T)[0].0;
        l1b_force(&mut f, seq).unwrap();
        let release = l1b_release_record(&f);
        assert!(release.forced);
        assert_eq!(release.detail.0.len(), 7);
        assert_eq!(
            release.detail.0[4].1,
            Text(
                if mismatch {
                    "digest_mismatch"
                } else {
                    "behind"
                }
                .into()
            )
        );
        f.reopen();
        assert!(f.raises(T).is_empty());
        f.close();
        if mismatch {
            let before = std::fs::metadata(f.path(T)).unwrap().len();
            std::fs::write(f.path(T), &short).unwrap();
            f.tail_without_runtime(T, [0x92; 16]);
            assert_eq!(std::fs::metadata(f.path(T)).unwrap().len(), before);
        } else {
            // Decode the real header to find its exact byte end, retaining no records.
            let mut header = std::io::Cursor::new(&short);
            crate::tape_log::store::Header::decode(&mut header, T).unwrap();
            std::fs::write(f.path(T), &short[..header.position() as usize]).unwrap();
        }
        f.reopen();
        assert!(
            f.raises(T).iter().any(|(_, raise)| raise
                .detail
                .0
                .iter()
                .any(|entry| entry.0.as_str() == "floor_sequence")),
            "changed bytes/extent must invalidate the floor waiver itself"
        );
        f.close();
        std::fs::write(f.path(T), &full).unwrap();
        f.reopen();
        // The restored bytes may differ from the last projected mark; rebuild is explicit.
        f.close();
        crate::StateHandle::rebuild_projection_from_config_file(&f.paths.config_path).unwrap();
        f.reopen();
        for (seq, _) in f.raises(T) {
            l1b_force(&mut f, seq).unwrap();
        }
        assert!(l1b_release_record(&f).detail.0.is_empty());
        assert!(f
            .rt()
            .0
            .authority
            .with_fold(|fold| fold.forced_observation[&T].0.is_empty()));
        f.reopen();
        assert!(f.raises(T).is_empty());
    }
    // A later received reference above a waived floor requires a new fence.
    {
        let mut f = Fixture::new();
        f.provision(T);
        let (short, snapshot) = f.native(T, false);
        let at = f.rt().native_reference(&snapshot).unwrap();
        f.name(6, at.clone());
        f.close();
        f.sql_loss();
        std::fs::write(f.path(T), short).unwrap();
        f.reopen();
        let sequence = f.raises(T)[0].0;
        l1b_force(&mut f, sequence).unwrap();
        f.reopen();
        assert!(f.raises(T).is_empty());
        f.name(
            6,
            lib::TapeAt {
                sequence: at.sequence + 1,
                record_digest: [0x77; 32],
                ..at.clone()
            },
        );
        f.reopen();
        assert_eq!(f.raises(T).len(), 1);
        assert_eq!(
            value(&f.raises(T)[0].1, "floor_sequence"),
            Unsigned(at.sequence + 1)
        );
    }
    // Sequence-zero floors are named by SupersedeTail, the schema's only zero-reference body.
    let mut f = Fixture::new();
    f.provision(T);
    {
        let authority =
            crate::TapeAuthority::open(f.paths.journal_dir.join("tapes"), T, &f.paths.sqlite_path)
                .unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        owner.publish_empty_prefix(262144, None).unwrap();
        let snapshot = owner.freeze().unwrap();
        let at = f.rt().native_reference(&snapshot).unwrap();
        assert_eq!(at.sequence, 0);
        f.rt().refresh_native(&snapshot).unwrap();
        f.name(0, at);
    }
    let full = std::fs::read(f.path(T)).unwrap();
    f.close();
    f.sql_loss();
    std::fs::remove_file(f.path(T)).unwrap();
    f.reopen();
    let seq = f.raises(T)[0].0;
    f.close();
    std::fs::write(f.path(T), full).unwrap();
    f.reopen();
    l1b_force(&mut f, seq).unwrap();
    assert!(l1b_release_record(&f).detail.0.is_empty());
}

/// In-process rebuild must retain catalog-only library evidence without appending a fence.
#[test]
fn k94_t13_in_process_older_library_refuses() {
    fn files(dir: &std::path::Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut result = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|n| n == "state.lock")
                || path.extension().is_some_and(|e| e == "lock")
            {
                continue;
            }
            if path.is_dir() {
                result.extend(files(&path));
            } else {
                let bytes = std::fs::read(&path).unwrap();
                result.insert(path, bytes);
            }
        }
        result
    }
    fn rows(path: &std::path::Path) -> Vec<Vec<rusqlite::types::Value>> {
        let conn =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let mut stmt=conn.prepare("select sequence,record_uuid,source_digest,tag,source_bytes from library_projection_records order by sequence").unwrap();
        let rows = stmt
            .query_map([], |r| (0..5).map(|i| r.get(i)).collect())
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows
    }
    let mut f = Fixture::new();
    f.provision(T);
    f.native(T, false);
    f.close();
    let old = files(&f.paths.audit_dir);
    f.reopen();
    f.rt()
        .append(|a, v| {
            a.fence_raised(
                lib::FenceRaised {
                    kind: lib::FenceKind::Write,
                    scope: lib::FenceScope::Tape(tape(T)),
                    cause: lib::FenceCause::PartialBatch,
                    evidence: None,
                    at: None,
                    operation: None,
                    detail: lib::Bounded(vec![]),
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    f.close();
    for p in files(&f.paths.audit_dir).keys() {
        std::fs::remove_file(p).unwrap();
    }
    for (p, b) in &old {
        std::fs::write(p, b).unwrap();
    }
    f.reopen();
    let before = files(&f.paths.audit_dir);
    let natives = files(&f.paths.journal_dir);
    let catalog = rows(&f.paths.sqlite_path);
    let error = f
        .state
        .as_mut()
        .unwrap()
        .rebuild_projection_from_tape_logs()
        .unwrap_err();
    assert!(
        error.to_string().contains("RebuildWouldDropLibraryRecords"),
        "{error}"
    );
    assert_eq!(files(&f.paths.audit_dir), before);
    assert_eq!(files(&f.paths.journal_dir), natives);
    assert_eq!(rows(&f.paths.sqlite_path), catalog);
}

/// A direct write drained under a cause-14 fence must be projected after release,
/// even when native projection previously gave the fenced tape a matching part.
#[test]
fn direct_write_to_fenced_tape_projects_after_release() {
    let mut f = Fixture::new();
    f.provision(T);
    let (short, snapshot) = f.native(T, false);
    f.rt().native_reference(&snapshot).unwrap();
    let full = std::fs::read(f.path(T)).unwrap();
    f.close();
    std::fs::write(f.path(T), &short).unwrap();
    let offline =
        crate::StateHandle::rebuild_projection_from_config_file(&f.paths.config_path).unwrap_err();
    assert!(offline.to_string().contains("RebuildWouldDropMarkEvidence"));
    f.reopen();
    let sequence = f.raises(T)[0].0;
    l1b_assert_refused_once(&mut f, sequence, 1, "live projection mark is above");
    f.close();
    std::fs::write(f.path(T), &full).unwrap();
    f.reopen();

    let counters = || {
        let tape = f
            .state
            .as_ref()
            .unwrap()
            .write_handle()
            .get_tape(&T)
            .unwrap()
            .unwrap();
        (
            tape.highest_protected_ordinal,
            tape.total_committed_ordinals,
            tape.last_committed_tape_file,
        )
    };
    let before = counters();
    let rt = f.rt();
    let native = {
        let _dispatch = rt.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        assert!(rt
            .0
            .authority
            .with_fold(|fold| native_floor::fenced(fold, &T)));
        rt.0.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot(T)
            .unwrap()
            .clone()
    };
    let part = Part::of(&native).unwrap();
    let snapshot = AuthoritySnapshot::new(native).unwrap();
    let result = rt.project_native(
        &mut f.state.as_ref().unwrap().write_handle(),
        &snapshot,
        ApplyPath::Live,
    );
    {
        let _dispatch = rt.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let matches =
            rt.0.stamp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|stamp| stamp.tapes.get(&T) == Some(&part));
        if result.is_ok() && matches {
            println!("T4 matching part supplied by project_native");
        } else {
            assert!(
                rt.projection_lag(T).is_none(),
                "T4 fallback requires no projection lag: {result:?}"
            );
            rt.0.stamp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .expect("T4 startup stamp")
                .tapes
                .insert(T, part);
            println!("T4 matching part supplied by permitted fallback: {result:?}");
        }
        assert_eq!(
            rt.0.stamp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .unwrap()
                .tapes
                .get(&T),
            Some(&part),
            "T4 matching part before mark"
        );
    }
    f.state
        .as_ref()
        .unwrap()
        .write_handle()
        .reconcile_tape_files_projection(T, &[], 7, 9)
        .unwrap();
    rt.take_projection_measurement();
    rt.catch_up().unwrap();
    assert_eq!(
        rt.take_projection_measurement().1.len(),
        0,
        "T4 fenced: 0 Apply"
    );
    assert_eq!(
        l1b_force(&mut f, sequence).unwrap().unwrap().state,
        "released"
    );
    rt.take_projection_measurement();
    rt.catch_up().unwrap();
    assert_eq!(
        rt.take_projection_measurement().1.len(),
        1,
        "T4 exactly 1 Apply after release"
    );
    let tape = f
        .state
        .as_ref()
        .unwrap()
        .write_handle()
        .get_tape(&T)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            tape.highest_protected_ordinal,
            tape.total_committed_ordinals,
            tape.last_committed_tape_file
        ),
        before
    );
}
