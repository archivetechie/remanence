//! Refusals counted in the library log (rung CODES, Appendix G): the helpers every site's test
//! shares, and the tests of the sites that need nothing but a typed root.
//!
//! A refusal answers as it did before, and one `RefusalRecorded` follows it: the code's number
//! and its own name, and the subject. The helpers read the log back from its bytes, so a record
//! that does not decode, or whose code or name is wrong, fails here and not only in a fold.

use super::{read_library_records, ChainRead, TypedRoot};
use remanence_state::library_log as lib;
use remanence_state::AuditActor;
use std::collections::BTreeMap;
use std::path::Path;

/// One `RefusalRecorded` as it decodes from the library log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordedRefusal {
    pub(crate) code: u64,
    pub(crate) name: String,
    pub(crate) subject_kind: String,
    pub(crate) subject_id: Option<String>,
    pub(crate) actor: AuditActor,
}

impl RecordedRefusal {
    /// What a recorded refusal must read as. The number and the name are spelled out here, so a
    /// wrong code or a misspelled name cannot agree with itself.
    pub(crate) fn expect(
        code: u64,
        name: &str,
        subject_kind: &str,
        subject_id: Option<[u8; 16]>,
        actor: AuditActor,
    ) -> Self {
        Self {
            code,
            name: name.to_owned(),
            subject_kind: subject_kind.to_owned(),
            subject_id: subject_id.map(|id| uuid::Uuid::from_bytes(id).to_string()),
            actor,
        }
    }
}

/// The refusal a record carries, when it is a `RefusalRecorded`. A record with the wrong detail
/// keys fails the test here, as the fold would refuse it.
pub(crate) fn decode_refusal(record: &lib::Record) -> Option<RecordedRefusal> {
    let diagnostic = record.diagnostic()?;
    if diagnostic.event != lib::DiagnosticEvent::RefusalRecorded {
        return None;
    }
    let find = |key: &str| {
        diagnostic
            .detail
            .0
            .iter()
            .find(|entry| entry.0.as_str() == key)
            .map(|entry| &entry.1)
    };
    let Some(lib::DiagnosticValue::Unsigned(code)) = find("code") else {
        panic!("a refusal record without an unsigned code: {diagnostic:?}");
    };
    let Some(lib::DiagnosticValue::Text(name)) = find("name") else {
        panic!("a refusal record without a text name: {diagnostic:?}");
    };
    assert_eq!(
        diagnostic.detail.0.len(),
        2,
        "a refusal record carries a code and a name and nothing else: {diagnostic:?}"
    );
    Some(RecordedRefusal {
        code: *code,
        name: name.as_str().to_owned(),
        subject_kind: diagnostic.subject_kind.as_str().to_owned(),
        subject_id: diagnostic
            .subject_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        actor: record.envelope.actor.clone(),
    })
}

/// One library-log record: its tag, and the refusal it decodes to when it is a `RefusalRecorded`.
pub(crate) type LogEntry = (lib::Tag, Option<RecordedRefusal>);

/// Replay the library log from its bytes once every runtime over it is gone. A competing owner is
/// retried every 10 ms for up to 5 s, as `TypedRoot::records` does.
pub(crate) fn read_log(
    paths: &remanence_state::StatePaths,
    library_uuid: [u8; 16],
) -> Vec<LogEntry> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match read_library_records(paths, library_uuid, &mut |record| {
            Some((record.tag(), decode_refusal(record)))
        }) {
            Ok(entries) => return entries,
            Err(ChainRead::Busy(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(ChainRead::Busy(errno)) => panic!(
                "the library log was still locked ({errno}) 5 s after the test dropped its \
                 states: a clone of the runtime outlived them"
            ),
            Err(ChainRead::Failed(message)) => panic!("{message}"),
        }
    }
}

/// The recorded refusals among `log`, in order.
pub(crate) fn refusals(log: &[LogEntry]) -> Vec<RecordedRefusal> {
    log.iter()
        .filter_map(|(_, refusal)| refusal.clone())
        .collect()
}

impl TypedRoot {
    /// Release the fixture's own handles, then read the whole library log back.
    pub(crate) fn refusal_log(&mut self) -> Vec<LogEntry> {
        self.release();
        read_log(&self.paths, self.library_uuid)
    }
}

/// The library log's last sequence and the catalog's rows, read while the runtime is alive.
pub(crate) struct LogMark {
    sequence: u64,
    sql: BTreeMap<String, Vec<String>>,
}

/// Tables in which the projection keeps its own books: which records it has applied, and how far
/// it has got. A record the projection applies and projects nothing from (a `Diagnostic`) adds a
/// row to the first three and moves the others, and changes no other table.
const PROJECTION_BOOKS: [&str; 6] = [
    "library_fold_progress",
    "library_projection_decisions",
    "library_projection_identity",
    "library_projection_progress",
    "library_projection_records",
    "projection_sources",
];

fn open_read_only(sqlite: &Path) -> rusqlite::Connection {
    let connection = rusqlite::Connection::open_with_flags(
        sqlite,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    connection
        .busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    connection
}

/// Every row of every table, sorted: the logical content of a catalog.
fn sql_rows(connection: &rusqlite::Connection) -> BTreeMap<String, Vec<String>> {
    let tables: Vec<String> = connection
        .prepare("select name from sqlite_master where type = 'table' order by name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    tables
        .into_iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("select * from \"{table}\""))
                .unwrap();
            let columns = statement.column_count();
            let mut rows: Vec<String> = statement
                .query_map([], |row| {
                    Ok(format!(
                        "{:?}",
                        (0..columns)
                            .map(|i| row.get::<_, rusqlite::types::Value>(i).unwrap())
                            .collect::<Vec<_>>()
                    ))
                })
                .unwrap()
                .map(Result::unwrap)
                .collect();
            rows.sort();
            (table, rows)
        })
        .collect()
}

/// Note where the library log stands before a refusal.
pub(crate) fn mark(sqlite: &Path) -> LogMark {
    let connection = open_read_only(sqlite);
    // The projection stores a sequence as eight big-endian bytes.
    let received: Vec<u8> = connection
        .query_row(
            "select received from projection_sources where source = 'library'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let sequence = u64::from_be_bytes(received.try_into().expect("an eight-byte sequence"));
    LogMark {
        sequence,
        sql: sql_rows(&connection),
    }
}

/// The library log grew by `records` records since `before`, the projection applied exactly that
/// many, and the catalog changed only in the tables where the projection keeps its own books. Read
/// while the runtime is alive: the projection is applied before the refusal answers.
pub(crate) fn assert_grew_by(sqlite: &Path, before: &LogMark, records: u64) {
    let after = mark(sqlite);
    assert_eq!(
        after.sequence,
        before.sequence + records,
        "the library log grew by {records} record(s)"
    );
    let applied = |mark: &LogMark| mark.sql["library_projection_records"].len();
    assert_eq!(
        applied(&after),
        applied(before) + usize::try_from(records).unwrap(),
        "the projection applied exactly the new record(s)"
    );
    let changed: Vec<&String> = after
        .sql
        .iter()
        .filter(|(table, rows)| before.sql.get(*table) != Some(*rows))
        .map(|(table, _)| table)
        .filter(|table| !PROJECTION_BOOKS.contains(&table.as_str()))
        .collect();
    assert!(
        changed.is_empty(),
        "the refusal changed catalog rows beyond the projection's own books: {changed:?}"
    );
}

fn reconcile_request(tape_uuid: Vec<u8>) -> tonic::Request<crate::pb::ReconcileTapeRequest> {
    let mut request = tonic::Request::new(crate::pb::ReconcileTapeRequest {
        tape_uuid,
        idempotency_key: None,
    });
    request
        .metadata_mut()
        .insert("x-remanence-actor", "operator-a".parse().unwrap());
    request
}

const RECONCILE_DEFERRED: &str = "ReconcileTapeDeferred: reconciling a tape from a physical scan is deferred; no tape was loaded or scanned";

/// Site 6, through the RPC. Every refusal is one record, naming the tape the request names and
/// the actor that asked, and nothing else is appended.
#[tokio::test]
async fn reconcile_tape_refusal_is_recorded_each_time_with_its_tape() {
    use crate::pb::catalog_server::Catalog;
    let mut typed = TypedRoot::open();
    let sqlite = typed.paths().sqlite_path.clone();
    let state = typed.api_state();
    let service = state.catalog_service();
    let before = mark(&sqlite);

    for refused_so_far in 1..=2 {
        let refused = Catalog::reconcile_tape(&service, reconcile_request(vec![0x5B; 16]))
            .await
            .expect_err("a tape cannot be reconciled from a scan yet");
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert_eq!(refused.message(), RECONCILE_DEFERRED);
        assert_grew_by(&sqlite, &before, refused_so_far);
    }
    assert!(
        state.index().unwrap().list_operations().unwrap().is_empty(),
        "the refusal accepted no operation"
    );
    crate::tests::assert_no_legacy_audit_records(&state);

    drop((service, state));
    let log = typed.refusal_log();
    let expected = RecordedRefusal::expect(
        6,
        "ReconcileTapeDeferred",
        "tape",
        Some([0x5B; 16]),
        AuditActor::Service("operator-a".into()),
    );
    assert_eq!(refusals(&log), [expected.clone(), expected]);
    assert!(
        log[log.len() - 2..]
            .iter()
            .all(|(tag, _)| *tag == lib::Tag::Diagnostic),
        "the two refusals are the last two records: {log:?}"
    );
}

/// Site 6: a request whose tape does not decode answers its decode error and records nothing.
#[tokio::test]
async fn reconcile_tape_with_an_undecodable_tape_records_nothing() {
    use crate::pb::catalog_server::Catalog;
    let mut typed = TypedRoot::open();
    let sqlite = typed.paths().sqlite_path.clone();
    let state = typed.api_state();
    let service = state.catalog_service();
    let before = mark(&sqlite);

    let refused = Catalog::reconcile_tape(&service, reconcile_request(vec![1, 2, 3]))
        .await
        .expect_err("three bytes are not a tape UUID");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        refused.message(),
        "tape_uuid must be a 16-byte UUID byte string"
    );
    assert_grew_by(&sqlite, &before, 0);

    drop((service, state));
    assert!(refusals(&typed.refusal_log()).is_empty());
}

/// D2, through the RPC. The projection of the record fails (a table it writes is gone), so the
/// append reports an error. The refusal still answers exactly as before, the failure is logged,
/// and the one record is the only effect: it is in the log.
#[tokio::test]
async fn a_refusal_whose_record_cannot_be_projected_still_answers_unchanged() {
    use crate::pb::catalog_server::Catalog;
    let mut typed = TypedRoot::open();
    let state = typed.api_state();
    let service = state.catalog_service();
    rusqlite::Connection::open(&typed.paths().sqlite_path)
        .unwrap()
        .execute_batch("drop table library_projection_progress")
        .unwrap();

    let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    crate::tests::ensure_log_capture_anchor();
    let guard = tracing::dispatcher::set_default(&tracing::dispatcher::Dispatch::new(
        super::WarningCaptureSubscriber::new(messages.clone()),
    ));
    let refused = Catalog::reconcile_tape(&service, reconcile_request(vec![0x5B; 16]))
        .await
        .expect_err("a tape cannot be reconciled from a scan yet");
    drop(guard);
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert_eq!(refused.message(), RECONCILE_DEFERRED);
    let warned = messages.lock().unwrap().clone();
    assert!(
        warned
            .iter()
            .any(|message| message.contains("the refusal was not recorded in the library log")),
        "the failure to record is logged: {warned:?}"
    );

    drop((service, state));
    assert_eq!(refusals(&typed.refusal_log()).len(), 1);
}

/// Site 9, through the production loop. The thread ends at its guard, so across the two poll
/// ticks that follow its return the log gains one record and no more.
#[test]
fn foreign_poll_disabled_is_recorded_once_across_two_poll_ticks() {
    use crate::drive_collection::{foreign_drive_poll_loop, AuditAppendContext};
    let mut typed = TypedRoot::open();
    let sqlite = typed.paths().sqlite_path.clone();
    let catalog = typed.catalog();
    let before = mark(&sqlite);
    let report = remanence_library::DiscoveryReport {
        unattached_drives: Vec::new(),
        complete: true,
        incomplete_evidence: Vec::new(),
        libraries: Vec::new(),
        warnings: Vec::new(),
    };
    let audit_dir = tempfile::tempdir().expect("audit tempdir");
    let audit = AuditAppendContext {
        dir: audit_dir.path().to_path_buf(),
        fsync: false,
        lock: std::sync::Arc::new(std::sync::Mutex::new(())),
    };
    let cadence = std::time::Duration::from_millis(50);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        foreign_drive_poll_loop(
            catalog,
            report,
            remanence_state::DrivesConfig::default(),
            std::collections::HashSet::new(),
            audit,
            cadence,
        );
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the foreign drive poll thread did not return within 5 s");
    worker.join().expect("foreign drive poll thread joined");
    // Two ticks of the loop's cadence pass; a loop that went on would poll, or record again.
    std::thread::sleep(cadence * 3);
    assert_grew_by(&sqlite, &before, 1);

    let log = typed.refusal_log();
    assert_eq!(
        refusals(&log),
        [RecordedRefusal::expect(
            9,
            "ForeignDrivePollDisabled",
            "library",
            None,
            AuditActor::System,
        )]
    );
    assert_eq!(log.last().unwrap().0, lib::Tag::Diagnostic);
}

/// The replay test. Refusals recorded through three production paths are read back from the
/// bytes after the store is reopened as a restarted daemon opens it: each decodes, its code is an
/// allocated one, and its name is that code's own.
#[tokio::test]
async fn recorded_refusals_replay_with_their_code_and_name_after_a_reopen() {
    use crate::drive_collection::{foreign_drive_poll_loop, AuditAppendContext};
    use crate::pb::catalog_server::Catalog;
    use crate::pb::library_service_server::LibraryService;
    let mut typed = TypedRoot::open();
    let state = typed.api_state();
    Catalog::reconcile_tape(&state.catalog_service(), reconcile_request(vec![0x5B; 16]))
        .await
        .expect_err("deferred");
    state
        .library_service()
        .annotate_drive(tonic::Request::new(crate::pb::AnnotateDriveRequest {
            drive_uuid: vec![0xD1; 16],
            purchase_date: String::new(),
            warranty_until: String::new(),
            cost: String::new(),
            note: "n".into(),
            notes_set: String::new(),
            allow_derived_identity: false,
        }))
        .await
        .expect_err("deferred");
    let catalog = typed.catalog();
    std::thread::spawn(move || {
        foreign_drive_poll_loop(
            catalog,
            remanence_library::DiscoveryReport {
                unattached_drives: Vec::new(),
                complete: true,
                incomplete_evidence: Vec::new(),
                libraries: Vec::new(),
                warnings: Vec::new(),
            },
            remanence_state::DrivesConfig::default(),
            std::collections::HashSet::new(),
            AuditAppendContext {
                dir: std::env::temp_dir(),
                fsync: false,
                lock: std::sync::Arc::new(std::sync::Mutex::new(())),
            },
            std::time::Duration::from_millis(50),
        )
    })
    .join()
    .unwrap();

    drop(state);
    typed.release();
    // Reopen: the open replays and folds the whole log, and refuses a record the fold refuses.
    let paths = typed.paths().clone();
    let lease =
        std::sync::Arc::new(remanence_state::StateLockGuard::acquire(&paths.state_dir).unwrap());
    let library_uuid = {
        let reopened =
            remanence_state::library_runtime::LibraryRuntime::open(&paths, true, lease.clone())
                .expect("the log with its refusals reopens");
        reopened.manifest().library_uuid()
    };
    drop(lease);
    let log = read_log(&paths, library_uuid);
    let recorded = refusals(&log);
    assert_eq!(
        recorded.iter().map(|r| r.code).collect::<Vec<_>>(),
        [6, 10, 9],
        "{recorded:?}"
    );
    for refusal in &recorded {
        let code = lib::RefusalCode::from_u32(u32::try_from(refusal.code).unwrap())
            .expect("an allocated code");
        assert_eq!(refusal.name, format!("{code:?}"));
    }
    assert_eq!(
        recorded.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        [
            "ReconcileTapeDeferred",
            "DriveStewardshipDeferred",
            "ForeignDrivePollDisabled"
        ]
    );
}
