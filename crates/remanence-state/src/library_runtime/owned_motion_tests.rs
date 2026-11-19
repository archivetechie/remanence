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
    /// Query MOVE ownership under the same dispatch lock as the runtime's callers.
    fn library_motion_owned(&self) -> bool {
        let _dispatch = self.runtime.0.dispatch.lock().unwrap();
        self.runtime
            .motion_scope_owned_locked(&lib::fold::Scope::Library("L".into()))
    }
    /// A completed closer stays quiet and cannot mask a later unowned motion on L.
    fn assert_later_unowned_motion_visible(&self, calls: &Mutex<Vec<String>>) {
        assert!(calls.lock().unwrap().is_empty(), "closer must not notify");
        let later = self.pending(move_command());
        self.runtime.unconfirmed(&later).unwrap();
        assert!(
            self.runtime.unowned_motion_unresolved("L"),
            "a stale owner must not mask the later unconfirmed motion"
        );
        assert_eq!(*calls.lock().unwrap(), vec!["L"]);
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

/// Fresh inventory after the fixture's MOVE: the source is empty and the bay is full.
fn moved_inventory() -> remanence_library::Library {
    use remanence_library::{DriveBay, ElementLayout, Library, Slot};
    let mut inquiry = [0; 36];
    inquiry[0] = 0x08; // Medium changer.
    inquiry[2] = 0x06; // SPC-4.
    inquiry[3] = 2; // SPC-3+ response data format.
    inquiry[4] = 31; // Remaining bytes of the standard INQUIRY response.
    Library {
        serial: "L".into(),
        changer_sg: "fixture-changer".into(),
        changer_sysfs: "fixture-sysfs".into(),
        changer_inquiry: remanence_library::scsi::Inquiry::parse(&inquiry).unwrap(),
        chassis_designator: None,
        layout: ElementLayout {
            robot_address: 0,
            drive_start: 0x100,
            drive_count: 1,
            slot_start: 0x400,
            slot_count: 1,
            ie_start: 0,
            ie_count: 0,
        },
        drive_bays: vec![DriveBay {
            element_address: 0x100,
            accessible: true,
            exception: None,
            installed: None,
            loaded: true,
            loaded_tape: Some("MOVED1L9".into()),
            source_slot: Some(0x400),
        }],
        slots: vec![Slot {
            element_address: 0x400,
            accessible: true,
            exception: None,
            full: false,
            cartridge: None,
        }],
        ie_ports: vec![],
    }
}

/// A successful MOVE releases its library owner without requesting reconciliation.
#[test]
fn confirm_inventory_releases_move_ownership_without_notifying() {
    let f = Fixture::new();
    let calls = f.notifications();
    let pending = f.pending(move_command());
    assert!(f.library_motion_owned());
    f.runtime
        .confirm_inventory(&pending, &moved_inventory())
        .unwrap();
    assert!(
        !f.library_motion_owned(),
        "confirmed MOVE releases ownership"
    );
    assert!(!f.runtime.requires_reconciliation(&pending.command));
    f.assert_later_unowned_motion_visible(&calls);
}

/// Standalone LOAD confirmation releases its endpoint owner without notifying a library.
/// The endpoint check pins the release; a standalone LOAD owns no library scope, so the later
/// library check is a guard only.
#[test]
fn confirm_standalone_releases_load_ownership_without_notifying() {
    let f = Fixture::new();
    let calls = f.notifications();
    let key = DriveKey::Standalone {
        drive_serial: "D".into(),
    };
    let pending = f.pending(MotionCommand::Load(key.clone()));
    assert!(f.runtime.endpoint_motion_owned(&key));
    f.runtime.confirm_standalone(&pending).unwrap();
    assert!(!f.runtime.endpoint_motion_owned(&key));
    assert!(!f.runtime.requires_reconciliation(&pending.command));
    f.assert_later_unowned_motion_visible(&calls);
}

/// A bay LOAD with a real readiness owner releases ownership when its ready probe closes it.
#[test]
fn confirm_readiness_releases_bay_load_ownership_without_notifying() {
    let f = Fixture::new();
    let calls = f.notifications();
    let key = bay(0x100);
    let operation = f
        .runtime
        .start_owned_operation(
            lib::OperationKind::MediaReadinessWait,
            lib::AuditActor::System,
        )
        .unwrap();
    f.runtime
        .begin_owned_readiness(&crate::MediaReadinessOperationInput {
            operation_id: uuid::Uuid::from_bytes(operation),
            run_id: None,
            endpoint: key.clone(),
            changer_sg: None,
            drive_sg: None,
            drive_serial: None,
            barcode: None,
            source_slot: Some(0x400),
            media_generation: None,
            media_family: None,
            phase: "readiness_poll".into(),
            state: "planned".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: None,
            evidence_path: None,
        })
        .unwrap();
    let command = MotionCommand::Load(key.clone());
    let context = MotionContext {
        parent_operation: Some(operation),
        load_confirmation: LoadConfirmation::Readiness,
        ..Default::default()
    };
    let pending = PendingMotion {
        ticket: f.runtime.intent(&command, &context).unwrap(),
        command,
        context,
        evidence: outcome::CompletionEvidence::success(),
    };
    assert!(f.runtime.endpoint_motion_owned(&key));
    f.runtime
        .confirm_readiness(
            &pending,
            &remanence_library::MediaReadiness::new(remanence_library::MediaReadinessKind::Ready),
            std::time::Duration::from_secs(1),
        )
        .unwrap();
    assert!(!f.runtime.endpoint_motion_owned(&key));
    assert!(!f.library_motion_owned());
    assert!(!f.runtime.requires_reconciliation(&pending.command));
    f.assert_later_unowned_motion_visible(&calls);
}

/// A known CHECK CONDITION closes a failed MOVE and releases its owner without notifying.
#[test]
fn failed_known_check_condition_releases_move_ownership_without_notifying() {
    let f = Fixture::new();
    let calls = f.notifications();
    let pending = f.pending(move_command());
    assert!(f.library_motion_owned());
    let completion = outcome::CompletionEvidence {
        dispatch: outcome::Dispatch::Issued,
        completion: outcome::Completion::Known,
        scsi_status: Some(2),
        failed: true,
        ..outcome::CompletionEvidence::success()
    };
    f.runtime.failed(pending.ticket, &completion).unwrap();
    assert!(!f.library_motion_owned());
    assert!(!f.runtime.requires_reconciliation(&pending.command));
    f.assert_later_unowned_motion_visible(&calls);
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
