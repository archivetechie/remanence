//! A LOAD is not a readiness operation (decision R1c-2). The SQL projector derives a readiness
//! row only from a `ReadinessObserved`; a LOAD intent projects into the motion table alone, and
//! moves only a row that a `ReadinessObserved` owns, whose head is open and not a fence. Every
//! fact below is published through `LibraryAuthority` and projected through `apply`, and the
//! no-row cases are repeated after the SQL projection is rebuilt from the library log alone.
use remanence_state::library_log::store::{Clock, Origin, UuidSource};
use remanence_state::{
    library_log::*,
    projector::{
        self,
        apply::{ApplyPath, AuthorityRecord},
        thread::rebuild_library,
    },
    CatalogIndex, StateLockGuard,
};
use std::{path::PathBuf, sync::Arc};
use uuid::Uuid;

struct Fixed;
impl Clock for Fixed {
    fn now(&self) -> u64 {
        1_793_664_000_000_000_000
    }
}
impl UuidSource for Fixed {
    fn next(&self, n: u64) -> Id {
        let mut id = [0x56; 16];
        id[8..].copy_from_slice(&n.to_be_bytes());
        id
    }
}
struct View;
impl TapeView for View {
    fn lookup(&self, _: &TapeAt) -> Result<TapeLookup, FoldError> {
        Ok(TapeLookup::NotYet)
    }
}
const LOG: [u8; 16] = [0xb2; 16];
fn bay(n: u16) -> DriveKey {
    DriveKey::Bay(Bay {
        library_serial: "R1C2-LIBRARY".into(),
        bay: n,
    })
}
/// The same bay as the catalog spells it.
fn sql_bay(n: u16) -> remanence_state::DriveKey {
    remanence_state::DriveKey::new("R1C2-LIBRARY", n)
}
fn evidence() -> CompletionEvidence {
    CompletionEvidence {
        dispatch: Dispatch::Issued,
        completion: Completion::Known,
        scsi_status: Some(0),
        host_status: Some(0),
        driver_status: Some(0),
        sense: None,
        residual: None,
    }
}

struct Fixture {
    root: tempfile::TempDir,
    lease: Arc<StateLockGuard>,
    authority: Option<LibraryAuthority>,
    writer: Option<CatalogIndex>,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let lease = Arc::new(StateLockGuard::acquire(root.path()).unwrap());
        let authority = Self::open(&root, &lease);
        let writer = projector::open(root.path().join("catalog.sqlite"))
            .unwrap()
            .write_handle();
        Self {
            root,
            lease,
            authority: Some(authority),
            writer: Some(writer),
        }
    }
    fn open(root: &tempfile::TempDir, lease: &Arc<StateLockGuard>) -> LibraryAuthority {
        LibraryAuthority::open(
            &root.path().join("library"),
            LOG,
            Origin {
                host: "test".into(),
                pid: 1,
                build: "r1c2".into(),
            },
            Box::new(Fixed),
            Box::new(Fixed),
            false,
            &View,
            lease.clone(),
        )
        .unwrap()
    }
    fn a(&self) -> &LibraryAuthority {
        self.authority.as_ref().unwrap()
    }
    fn sql(&self) -> &CatalogIndex {
        self.writer.as_ref().unwrap()
    }
    fn apply(&mut self, r: AuthorityReceipt) -> DecisionRef {
        let reference = DecisionRef {
            sequence: r.receipt.sequence,
            record_uuid: r.receipt.record_uuid,
        };
        for applied in r.applied {
            self.writer
                .as_mut()
                .unwrap()
                .apply(
                    &AuthorityRecord::Library(Box::new(applied)),
                    ApplyPath::Live,
                )
                .unwrap();
        }
        reference
    }
    /// Delete the SQL projection and rebuild it from the library log alone.
    fn rebuild(&mut self) {
        self.authority.take();
        self.writer.take();
        let db = self.root.path().join("catalog.sqlite");
        for p in [
            db.clone(),
            PathBuf::from(format!("{}-wal", db.display())),
            PathBuf::from(format!("{}-shm", db.display())),
        ] {
            match std::fs::remove_file(p) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => panic!("{e}"),
            }
        }
        let mut writer = projector::open(&db).unwrap().write_handle();
        let report = rebuild_library(
            &mut writer,
            &self.root.path().join("library"),
            LOG,
            &View,
            ApplyPath::Rebuild,
        )
        .unwrap();
        assert!(report.pending.is_empty());
        self.writer = Some(writer);
        self.authority = Some(Self::open(&self.root, &self.lease));
    }
    fn load(&mut self, parent: Option<Id>, endpoint: DriveKey, attempt: u32) -> (Id, DecisionRef) {
        let command = *Uuid::new_v4().as_bytes();
        let receipt = self
            .a()
            .motion_intent(
                MotionIntent {
                    command,
                    parent_operation: parent,
                    motion: Motion::Load(Load { endpoint, attempt }),
                    tape: None,
                    inventory_generation: None,
                    readiness_generation: None,
                },
                AuditActor::System,
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        (command, self.apply(receipt))
    }
    /// A readiness owner's first observation, in `state` with `result`.
    fn begin(
        &mut self,
        endpoint: DriveKey,
        state: ReadinessState,
        generation: u64,
    ) -> (Id, DecisionRef) {
        let result = match state {
            ReadinessState::Ready => ReadinessResult::Ready,
            _ => ReadinessResult::Pending,
        };
        let (operation, receipt) = self
            .a()
            .begin_readiness(
                ReadinessObserved {
                    source_slot: None,
                    run: None,
                    cancel_source: None,
                    scope: ReadinessScope::Endpoint,
                    media: None,
                    operation: [0; 16],
                    endpoint,
                    tape: None,
                    barcode: Some("R1C2001L9".into()),
                    phase: ReadinessPhase::ReadinessPoll,
                    state,
                    previous: None,
                    started_unix_ns: Fixed.now(),
                    limit_ms: Some(60_000),
                    evidence: None,
                    load_intent: None,
                    ready_generation: generation,
                    result,
                },
                AuditActor::System,
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        (operation, self.apply(receipt))
    }
    /// The next observation of an owner, on the same endpoint.
    fn observe(
        &mut self,
        operation: Id,
        endpoint: DriveKey,
        previous: DecisionRef,
        state: ReadinessState,
        generation: u64,
    ) -> DecisionRef {
        let receipt = self
            .a()
            .readiness_observed(
                ReadinessObserved {
                    source_slot: None,
                    run: None,
                    cancel_source: None,
                    scope: ReadinessScope::Endpoint,
                    media: None,
                    operation,
                    endpoint,
                    tape: None,
                    barcode: Some("R1C2001L9".into()),
                    phase: ReadinessPhase::ReadinessPoll,
                    state,
                    previous: Some(previous),
                    started_unix_ns: Fixed.now(),
                    limit_ms: Some(60_000),
                    evidence: None,
                    load_intent: None,
                    ready_generation: generation,
                    result: ReadinessResult::Pending,
                },
                AuditActor::System,
                SourceLayer::Layer4,
                &View,
            )
            .unwrap();
        self.apply(receipt)
    }
    fn row_state(&self, operation: Id) -> Option<String> {
        self.sql()
            .media_readiness_operation(Uuid::from_bytes(operation))
            .unwrap()
            .map(|row| row.state)
    }
    fn active_rows(&self) -> usize {
        self.sql()
            .list_active_media_readiness_operations(None)
            .unwrap()
            .len()
    }
    /// The motion table kept the LOAD, whatever the readiness table did.
    fn motion_rows(&self, endpoint: &DriveKey) -> i64 {
        let sql_key = |endpoint: &DriveKey| match endpoint {
            DriveKey::Bay(b) => {
                remanence_state::DriveKey::new(b.library_serial.as_str(), b.bay).encode()
            }
            DriveKey::Standalone(_) => unreachable!("bays only"),
        };
        let conn = rusqlite::Connection::open_with_flags(
            self.root.path().join("catalog.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        conn.query_row(
            "select count(*) from library_motion where endpoint = ?1",
            [sql_key(endpoint)],
            |row| row.get(0),
        )
        .unwrap()
    }
}

/// The rehearsal's defect: a LOAD with no parent (the CLI writer's) made an open readiness row
/// that no record owned, which blocked its endpoint and every start. It projects no row now,
/// live and after a rebuild, and the projected motion intent is kept.
#[test]
fn an_unowned_load_intent_projects_no_readiness_row() {
    let mut f = Fixture::new();
    let (command, _) = f.load(None, bay(1), 1);
    assert_eq!(f.row_state(command), None, "no row under the command id");
    assert_eq!(f.active_rows(), 0);
    assert!(f
        .sql()
        .media_readiness_admission_conflicts(&sql_bay(1), None, None, false)
        .unwrap()
        .is_empty());
    assert_eq!(f.motion_rows(&bay(1)), 1);
    f.rebuild();
    assert_eq!(f.row_state(command), None);
    assert_eq!(f.active_rows(), 0);
    assert_eq!(f.motion_rows(&bay(1)), 1);
}

/// The daemon's LoadDrive `no_wait` and cleaning LOADs carry a parent operation that no
/// `ReadinessObserved` names. The parent id must not become a readiness row either.
#[test]
fn a_load_parented_to_an_operation_no_readiness_record_names_projects_no_row() {
    let mut f = Fixture::new();
    // The parent is a robotics-style operation: opened, but never a readiness owner.
    let (parent, receipt) = f
        .a()
        .start_operation(
            OperationKind::LoadDrive,
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(receipt);
    let (command, _) = f.load(Some(parent), bay(1), 1);
    assert_eq!(f.row_state(parent), None, "no row under the parent id");
    assert_eq!(f.row_state(command), None);
    assert_eq!(f.active_rows(), 0);
    assert_eq!(f.motion_rows(&bay(1)), 1);
    f.rebuild();
    assert_eq!(f.row_state(parent), None);
    assert_eq!(f.active_rows(), 0);
}

/// Contrast: a LOAD parented to an open readiness operation that a `ReadinessObserved` owns
/// still records its write-ahead `pre_ready_loading` on that row.
#[test]
fn a_load_parented_to_an_open_owned_readiness_row_records_pre_ready_loading() {
    let mut f = Fixture::new();
    let (operation, _) = f.begin(bay(1), ReadinessState::Planned, 1);
    assert_eq!(f.row_state(operation).as_deref(), Some("planned"));
    f.load(Some(operation), bay(1), 1);
    assert_eq!(f.row_state(operation).as_deref(), Some("pre_ready_loading"));
    f.rebuild();
    assert_eq!(f.row_state(operation).as_deref(), Some("pre_ready_loading"));
    assert_eq!(f.active_rows(), 1);
}

/// A LOAD parented to a fenced readiness operation must leave the row unchanged: the projector
/// writes SQL directly, so moving a fence to `pre_ready_loading` would mask it. A row whose head
/// is already `ready` is not reopened either.
#[test]
fn a_load_parented_to_a_fenced_or_ready_readiness_row_leaves_the_row_unchanged() {
    for fence in [
        ReadinessState::AbortedUnknown,
        ReadinessState::TimeoutUnknown,
        ReadinessState::TransportUnknown,
        ReadinessState::TerminalError,
        ReadinessState::ReservationConflict,
    ] {
        let mut f = Fixture::new();
        let (operation, first) = f.begin(bay(1), ReadinessState::Planned, 1);
        f.observe(operation, bay(1), first, fence, 2);
        let before = f.row_state(operation);
        assert!(
            matches!(
                before.as_deref(),
                Some(
                    "aborted_unknown"
                        | "timeout_unknown"
                        | "transport_unknown"
                        | "terminal_error"
                        | "reservation_conflict"
                )
            ),
            "{before:?}"
        );
        f.load(Some(operation), bay(1), 1);
        assert_eq!(f.row_state(operation), before, "{fence:?} stays a fence");
        f.rebuild();
        assert_eq!(f.row_state(operation), before, "{fence:?} after rebuild");
        assert_eq!(f.active_rows(), 1);
    }
    let mut f = Fixture::new();
    let (operation, _) = f.begin(bay(1), ReadinessState::Ready, 1);
    assert_eq!(f.row_state(operation).as_deref(), Some("ready"));
    f.load(Some(operation), bay(1), 1);
    assert_eq!(f.row_state(operation).as_deref(), Some("ready"));
    assert_eq!(f.active_rows(), 0);
}

/// The fold answers per endpoint (decision R1c-3): an open LOAD names its own bay only, a
/// library-scope MOVE names no endpoint, and `Failed` or a confirmed `Done` clears the bay.
#[test]
fn the_fold_names_unresolved_motion_per_endpoint() {
    let mut f = Fixture::new();
    let load_motion = |n| {
        Motion::Load(Load {
            endpoint: bay(n),
            attempt: 1,
        })
    };
    assert!(!f.a().endpoint_motion_unresolved(&bay(1)));
    let (_, open) = f.load(None, bay(1), 1);
    assert!(f.a().endpoint_motion_unresolved(&bay(1)));
    assert!(
        !f.a().endpoint_motion_unresolved(&bay(2)),
        "another bay is not named"
    );
    // The library scope is unresolved for every bay, which is the motion gate's question.
    assert!(f.a().motion_requires_reconciliation(&load_motion(2)));
    let outcome = |intent: DecisionRef, result| MotionOutcome {
        intent,
        result,
        evidence: evidence(),
        confirmation: None,
    };
    let failed = f
        .a()
        .motion_outcome(
            outcome(open, MotionResult::Failed),
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(failed);
    assert!(
        !f.a().endpoint_motion_unresolved(&bay(1)),
        "Failed is a known result"
    );
    assert!(!f.a().motion_requires_reconciliation(&load_motion(2)));

    let (_, open) = f.load(None, bay(1), 1);
    let unknown = f
        .a()
        .motion_outcome(
            outcome(open, MotionResult::Unknown),
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    f.apply(unknown);
    assert!(
        f.a().endpoint_motion_unresolved(&bay(1)),
        "Unknown stays unresolved"
    );
    let observed = f
        .a()
        .inventory_reconciled(
            InventoryReconciled {
                subject: InventorySubject::Library("R1C2-LIBRARY".into()),
                observation_generation: 1,
                resolves: Bounded(vec![]),
                complete: true,
                elements: Bounded(vec![]),
            },
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    let observed_at = f.apply(observed);
    let mut done = outcome(open, MotionResult::Done);
    done.confirmation = Some(Confirmation::Inventory(observed_at.sequence));
    assert!(
        f.a()
            .motion_outcome(done, AuditActor::System, SourceLayer::Layer4, &View)
            .is_err(),
        "a Done after Unknown is refused by the fold"
    );
    assert!(f.a().endpoint_motion_unresolved(&bay(1)));

    // A library-scope MOVE names no endpoint.
    let mut g = Fixture::new();
    let receipt = g
        .a()
        .motion_intent(
            MotionIntent {
                command: *Uuid::new_v4().as_bytes(),
                parent_operation: None,
                motion: Motion::Move(Move {
                    library_serial: "R1C2-LIBRARY".into(),
                    source: 0x400,
                    destination: 0x100,
                }),
                tape: None,
                inventory_generation: None,
                readiness_generation: None,
            },
            AuditActor::System,
            SourceLayer::Layer4,
            &View,
        )
        .unwrap();
    g.apply(receipt);
    assert!(!g.a().endpoint_motion_unresolved(&bay(1)));
    assert!(g.a().motion_requires_reconciliation(&load_motion(1)));
}

/// The startup refusal for a transition on an operation no `ReadinessObserved` owns names the
/// operation id, and "journal replay failed" appears once, not once per layer that passed it on.
#[test]
fn a_transition_on_an_ownerless_operation_names_it_and_prefixes_once() {
    use remanence_state::{
        library_runtime::LibraryRuntime, MediaReadinessTransitionInput, StateError, StatePaths,
    };
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let paths = StatePaths {
        config_path: root.join("config"),
        state_dir: root.join("state"),
        audit_dir: root.join("library"),
        journal_dir: root.join("native"),
        sqlite_path: root.join("catalog.sqlite"),
        tape_cache_dir: root.join("cache"),
        calibration_dir: root.join("calibration"),
    };
    let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
    let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
    let mut index = projector::open(&paths.sqlite_path)
        .unwrap()
        .with_library_runtime(runtime)
        .write_handle();
    let operation = Uuid::new_v4();
    let error = index
        .record_media_readiness_transition(MediaReadinessTransitionInput {
            completion_evidence: None,
            operation_id: operation,
            phase: None,
            state: "aborted_unknown".into(),
            dirty_scope: None,
            last_cdb_opcode: None,
            last_sense_raw: None,
            last_sense_key: None,
            last_asc: None,
            last_ascq: None,
            last_host_status: None,
            last_driver_status: None,
            target_status: None,
            transport_class: None,
            cancel_source: None,
            signal: None,
            evidence_path: None,
            last_error_json: None,
            quarantine_id: None,
        })
        .unwrap_err();
    assert!(
        matches!(error, StateError::JournalReplayFailed(_)),
        "{error:?}"
    );
    let text = error.to_string();
    assert!(text.contains("no authenticated owner"), "{text}");
    assert!(text.contains(&operation.to_string()), "{text}");
    assert_eq!(text.matches("journal replay failed").count(), 1, "{text}");
}
