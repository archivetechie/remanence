//! Process-local ownership preserves live motions through reconciliation and releases
//! only at outcomes; notifier callbacks must run after the runtime's locks drop.
use super::*;
use crate::{StateLockGuard, StatePaths};
use remanence_library::DriveKey;
use std::sync::{Arc, Mutex};

struct Fixture {
    runtime: LibraryRuntime,
    _root: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
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
        let runtime = LibraryRuntime::open(
            &paths,
            true,
            Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap()),
        )
        .unwrap();
        Self {
            runtime,
            _root: root,
        }
    }
    fn notifications(&self) -> Arc<Mutex<Vec<String>>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let captured = calls.clone();
        let weak = Arc::downgrade(&self.runtime.0);
        self.runtime.set_motion_reconcile_notifier(move |serial| {
            let inner = weak.upgrade().unwrap();
            let _dispatch = inner
                .dispatch
                .try_lock()
                .expect("notifier holds no dispatch");
            let _owned = inner
                .owned_motion
                .try_lock()
                .expect("notifier holds no owned set");
            let _slot = inner
                .motion_reconcile_notifier
                .try_lock()
                .expect("notifier holds no slot");
            captured.lock().unwrap().push(serial.to_owned());
        });
        calls
    }
    fn pending(&self, command: MotionCommand) -> PendingMotion {
        let context = MotionContext::default();
        let ticket = self.runtime.intent(&command, &context).unwrap();
        PendingMotion {
            command,
            context,
            ticket,
            evidence: outcome::CompletionEvidence::success(),
        }
    }
    fn reconcile(&self) {
        self.runtime
            .reconcile_motion_observation(lib::InventorySubject::Library("L".into()), vec![])
            .unwrap();
    }
}
fn move_command() -> MotionCommand {
    MotionCommand::Move {
        library_serial: "L".into(),
        source: 0x400,
        destination: 0x100,
    }
}
fn bay(bay: u16) -> DriveKey {
    DriveKey::Bay {
        library_serial: "L".into(),
        bay,
    }
}
fn unknown() -> outcome::CompletionEvidence {
    outcome::CompletionEvidence {
        completion: outcome::Completion::Unknown,
        scsi_status: None,
        failed: true,
        ..outcome::CompletionEvidence::success()
    }
}

#[test]
fn t1_move_is_owned_until_unknown_and_notifies_outside_dispatch() {
    let f = Fixture::new();
    let calls = f.notifications();
    let pending = f.pending(move_command());
    assert!(
        !f.runtime.unowned_motion_unresolved("L"),
        "T1 owned MOVE is not unowned"
    );
    assert!(f.runtime.requires_reconciliation(&pending.command));
    assert!(calls.lock().unwrap().is_empty());
    f.runtime.failed(pending.ticket, &unknown()).unwrap();
    assert!(f.runtime.unowned_motion_unresolved("L"));
    assert_eq!(*calls.lock().unwrap(), vec!["L"]);
    assert!(!f.runtime.motion_fenced());
}

#[test]
fn t2_inventory_preserves_owned_load_then_resolves_unknown() {
    let f = Fixture::new();
    let key = bay(0x100);
    let pending = f.pending(MotionCommand::Load(key.clone()));
    assert!(f.runtime.endpoint_motion_owned(&key));
    f.reconcile();
    assert!(
        f.runtime.endpoint_motion_unresolved(&key),
        "T2 owned LOAD left unresolved"
    );
    assert!(f.runtime.requires_reconciliation(&pending.command));
    f.runtime.unconfirmed(&pending).unwrap();
    assert!(!f.runtime.endpoint_motion_owned(&key));
    f.reconcile();
    assert!(!f.runtime.endpoint_motion_unresolved(&key));
    assert!(!f.runtime.unowned_motion_unresolved("L"));
    assert!(!f.runtime.requires_reconciliation(&pending.command));
}

#[test]
fn t3_stale_closer_releases_ownership_on_semantic_refusal() {
    let f = Fixture::new();
    let key = bay(0x100);
    let first = f.pending(MotionCommand::Load(key.clone()));
    // A direct authority append deliberately bypasses the funnel's ownership filter.
    f.runtime
        .append(|authority, view| {
            let subject = lib::InventorySubject::Library("L".into());
            authority.inventory_reconciled(
                lib::InventoryReconciled {
                    observation_generation: authority.next_inventory_generation(&subject)?,
                    subject,
                    resolves: lib::Bounded(vec![lib::DecisionRef {
                        sequence: first.ticket.sequence,
                        record_uuid: first.ticket.record_uuid,
                    }]),
                    complete: true,
                    elements: lib::Bounded(vec![]),
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                view,
            )
        })
        .unwrap();
    let second = f.pending(MotionCommand::Load(bay(0x101)));
    f.runtime.unconfirmed(&second).unwrap();
    assert!(f.runtime.endpoint_motion_owned(&key));
    assert!(
        !f.runtime.unowned_motion_unresolved("L"),
        "stale owner masks the unowned second LOAD"
    );
    let error = f.runtime.failed(first.ticket, &unknown()).unwrap_err();
    assert_eq!(error.kind, MotionAuthorityFailure::SemanticRefusal);
    assert!(!f.runtime.endpoint_motion_owned(&key));
    assert!(f.runtime.unowned_motion_unresolved("L"));
    assert!(!f.runtime.motion_fenced());
}

#[test]
fn t4_refused_intent_notifies_only_for_an_unowned_library_blocker() {
    let f = Fixture::new();
    let calls = f.notifications();
    let pending = f.pending(move_command());
    let refused = || {
        f.runtime
            .intent(&MotionCommand::Load(bay(0x100)), &MotionContext::default())
            .unwrap_err()
    };
    assert_eq!(refused().kind, MotionAuthorityFailure::UnresolvedMotion);
    assert!(calls.lock().unwrap().is_empty());
    f.runtime.unconfirmed(&pending).unwrap();
    calls.lock().unwrap().clear();
    assert_eq!(refused().kind, MotionAuthorityFailure::UnresolvedMotion);
    assert_eq!(*calls.lock().unwrap(), vec!["L"]);
}

#[test]
fn t5_endpoint_query_notifies_only_for_unowned_bay_motion() {
    let f = Fixture::new();
    let calls = f.notifications();
    let key = bay(0x100);
    let pending = f.pending(MotionCommand::Load(key.clone()));
    assert!(f.runtime.endpoint_motion_unresolved(&key));
    assert!(calls.lock().unwrap().is_empty());
    f.runtime.unconfirmed(&pending).unwrap();
    assert_eq!(*calls.lock().unwrap(), vec!["L"]);
    calls.lock().unwrap().clear();
    assert!(!f.runtime.endpoint_motion_unresolved(&bay(0x101)));
    assert!(calls.lock().unwrap().is_empty());
    assert!(f.runtime.endpoint_motion_unresolved(&key));
    assert_eq!(*calls.lock().unwrap(), vec!["L"]);
    calls.lock().unwrap().clear();

    let standalone = DriveKey::Standalone {
        drive_serial: "D".into(),
    };
    let pending = f.pending(MotionCommand::Load(standalone.clone()));
    assert!(f.runtime.endpoint_motion_owned(&standalone));
    assert!(f.runtime.endpoint_motion_unresolved(&standalone));
    assert!(calls.lock().unwrap().is_empty());
    f.runtime.unconfirmed(&pending).unwrap();
    assert!(!f.runtime.endpoint_motion_owned(&standalone));
    assert!(f.runtime.endpoint_motion_unresolved(&standalone));
    let error = f
        .runtime
        .intent(&pending.command, &MotionContext::default())
        .unwrap_err();
    assert_eq!(error.kind, MotionAuthorityFailure::UnresolvedMotion);
    assert!(calls.lock().unwrap().is_empty());
    f.runtime.reconcile_standalone(&standalone).unwrap();
    assert!(!f.runtime.endpoint_motion_unresolved(&standalone));
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn t6_history_unavailable_latches_motion_for_bare_and_wrapped_errors() {
    for wrapped in [false, true] {
        let f = Fixture::new();
        let mut error = lib::AuthorityError::Store(lib::store::StoreError::Fold(
            lib::FoldError::HistoryUnavailable,
        ));
        if wrapped {
            error = lib::AuthorityError::AppliedBeforeRefusal {
                error: Box::new(error),
                applied: vec![],
            };
        }
        assert!(!f.runtime.motion_fenced());
        assert_eq!(
            f.runtime.motion_error(error.into()).kind,
            MotionAuthorityFailure::StoreFailure
        );
        assert!(f.runtime.motion_fenced());
        assert_eq!(
            MotionAuthority::ensure_motion_allowed(&f.runtime)
                .unwrap_err()
                .kind,
            MotionAuthorityFailure::StoreFailure
        );
    }
}
