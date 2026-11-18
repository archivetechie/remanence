//! Projection stamps preserve durable gaps and skip work only for copied prefixes.
use super::*;
use crate::authority::TapeAuthorityView;
use crate::tape_log::{authority::AuthoritySnapshot, CommitCursor, SessionEndReason};

const X: lib::Id = [0x61; 16];
const Y: lib::Id = [0x62; 16];

struct Fixture {
    state: Option<crate::StateHandle>,
    paths: StatePaths,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config_text = format!(
            r#"[daemon]
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
        let paths = StatePaths::from_config(root.path().join("config.toml"), &config);
        let state = crate::StateHandle::open_with_config(paths.clone(), config.clone()).unwrap();
        Self {
            state: Some(state),
            paths,
            _root: root,
        }
    }

    fn runtime(&self) -> LibraryRuntime {
        self.state.as_ref().unwrap().library_runtime()
    }

    fn provision(&self, id: lib::Id) -> lib::Receipt {
        self.runtime()
            .append(|a, v| {
                a.reprovision(
                    lib::Reprovision {
                        tape: lib::TapeRef {
                            uuid: id,
                            incarnation: 1,
                        },
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
            .unwrap()
    }
}

/// Append a real library record without returning its application to the runtime.
fn diagnostic(a: &LibraryAuthority, v: &dyn TapeView) -> Result<AuthorityReceipt, AuthorityError> {
    a.diagnostic(
        lib::Diagnostic {
            event: lib::DiagnosticEvent::RequestReceived,
            subject_kind: "stamp-test".into(),
            subject_id: None,
            detail: lib::Bounded(vec![]),
        },
        lib::AuditActor::System,
        lib::SourceLayer::Layer4,
        v,
    )
}

impl Fixture {
    fn has_record(&self, sequence: u64) -> bool {
        rusqlite::Connection::open(&self.paths.sqlite_path)
            .unwrap()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM library_projection_records WHERE sequence = ?1)",
                [sequence.to_be_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap()
    }
}

/// A durable write followed by R6's fold error stays dirty until the next append heals it.
#[test]
fn r6_write_keeps_projection_dirty() {
    let f = Fixture::new();
    let rt = f.runtime();
    let sequence = std::cell::Cell::new(0);
    let result = rt.append(|a, v| {
        sequence.set(diagnostic(a, v)?.receipt.sequence);
        Err(AuthorityError::Store(StoreError::Fold(
            lib::FoldError::HistoryUnavailable,
        )))
    });
    assert!(result.is_err());
    assert!(
        rt.0.projection_dirty.load(Ordering::Acquire),
        "R6 projection_dirty set"
    );
    assert!(!rt.motion_fenced());
    assert!(!f.has_record(sequence.get()));
    let before = rt.0.library_passes.load(Ordering::Relaxed);
    rt.append(diagnostic).unwrap();
    assert_eq!(
        rt.0.library_passes.load(Ordering::Relaxed) - before,
        1,
        "R6 one pass"
    );
    assert!(f.has_record(sequence.get()));
}

/// An identical retry returns the original receipt without a catch-up pass.
#[test]
fn duplicate_does_not_catch_up() {
    let f = Fixture::new();
    let original = f.provision(X);
    let rt = f.runtime();
    let before = rt.0.library_passes.load(Ordering::Relaxed);
    let retried = f.provision(X);
    assert_eq!(original, retried);
    assert_eq!(
        rt.0.library_passes.load(Ordering::Relaxed) - before,
        0,
        "duplicate zero passes"
    );
    assert!(!rt.0.projection_dirty.load(Ordering::Acquire));
}

impl Fixture {
    fn native(&self, id: lib::Id) -> AuthoritySnapshot {
        self.provision(id);
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
        owner.freeze().unwrap()
    }

    fn advance(&self, id: lib::Id) -> AuthoritySnapshot {
        let authority = crate::TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            id,
            &self.paths.sqlite_path,
        )
        .unwrap();
        let mut owner = authority.acquire_exclusive().unwrap();
        owner
            .open_session(
                id,
                "2026-11-12T00:00:00Z".into(),
                crate::tape_log::SessionDrive {
                    serial: "DRV-TEST".into(),
                    vendor: None,
                    product: None,
                    endpoint: lib::DriveKey::Standalone(lib::Standalone {
                        drive_serial: "DRV-TEST".into(),
                    }),
                    drive_uuid: None,
                },
            )
            .unwrap();
        owner
            .end_session(
                id,
                "2026-11-12T00:00:01Z".into(),
                SessionEndReason::CleanClose,
                None,
                std::iter::empty(),
            )
            .unwrap();
        owner.freeze().unwrap()
    }
}

/// Measure only the catch-up itself; every setup action precedes this boundary.
fn measured_catch_up(rt: &LibraryRuntime) -> (usize, usize, usize) {
    rt.take_projection_measurement();
    let passes = rt.0.library_passes.load(Ordering::Relaxed);
    let frames = rt.0.ack_frames_checked.load(Ordering::Relaxed);
    rt.catch_up().unwrap();
    (
        rt.0.library_passes.load(Ordering::Relaxed) - passes,
        rt.take_projection_measurement().1.len(),
        rt.0.ack_frames_checked.load(Ordering::Relaxed) - frames,
    )
}

/// Unchanged, changed, library-only and dirty catch-ups visit exactly their work.
#[test]
fn catch_up_projects_only_changed_components() {
    let f = Fixture::new();
    let rt = f.runtime();
    let s0 = f.native(X);
    rt.refresh_native(&s0).unwrap();
    rt.catch_up().unwrap();
    assert_eq!(measured_catch_up(&rt), (0, 0, 0), "T2(a) unchanged");

    let next = f.advance(X);
    rt.refresh_native(&next).unwrap();
    let (passes, applies, frames) = measured_catch_up(&rt);
    assert_eq!(passes, 0, "T2(b) zero library passes");
    assert_eq!(applies, 1, "T2(b) one Apply");
    assert!(
        frames < next.tape_summary().last_closed_sequence as usize,
        "T2(b) strictly fewer frames than a full walk"
    );

    f.provision(Y);
    assert_eq!(
        measured_catch_up(&rt),
        (0, 0, 0),
        "T2(c) library append already projected"
    );
    rt.fixture_mark_projection_dirty();
    assert_eq!(measured_catch_up(&rt), (1, 1, 0), "T2(d) dirty catch-up");

    let s1 = f.advance(X);
    rt.project_native(&mut f.index(), &s1, ApplyPath::Live)
        .unwrap();
    let (passes, applies, frames) = measured_catch_up(&rt);
    assert_eq!(applies, 0, "T2(e) zero Apply after project_native");
    assert_eq!((passes, frames), (0, 0));

    let s2 = f.advance(X);
    rt.refresh_native(&s2).unwrap();
    assert!(s1.tape_summary().last_closed_sequence > s0.tape_summary().last_closed_sequence);
    assert!(s2.tape_summary().last_closed_sequence > s1.tape_summary().last_closed_sequence);
    rt.project_native(&mut f.index(), &s0, ApplyPath::Live)
        .unwrap();
    let (passes, applies, frames) = measured_catch_up(&rt);
    assert!(frames > 0, "T2(f) more than zero frames beyond the mark");
    assert_eq!(applies, 1, "T2(f) one Apply for unchecked current prefix");
    assert_eq!(passes, 0);
}

/// A panic cannot let the next successful append stamp past an unprojected record.
#[test]
fn panic_write_leaves_contiguous_library_gap() {
    let f = Fixture::new();
    let rt = f.runtime();
    let sequence = std::cell::Cell::new(0);
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = rt.append(|a, v| {
            sequence.set(diagnostic(a, v)?.receipt.sequence);
            panic!("injected panic after durable library write");
        });
    }));
    assert!(unwind.is_err());
    assert!(!rt.0.projection_dirty.load(Ordering::Acquire));
    assert!(!f.has_record(sequence.get()));
    rt.append(diagnostic).unwrap();
    let before = rt.0.library_passes.load(Ordering::Relaxed);
    rt.catch_up().unwrap();
    assert_eq!(
        rt.0.library_passes.load(Ordering::Relaxed) - before,
        1,
        "panic gap one pass"
    );
    assert!(f.has_record(sequence.get()));
}

impl Fixture {
    fn index(&self) -> crate::CatalogIndex {
        self.state.as_ref().unwrap().write_handle()
    }

    fn sql(&self, sql: &str) {
        rusqlite::Connection::open(&self.paths.sqlite_path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    fn fail(&self, tape: lib::Id) {
        self.sql(&format!(
            "CREATE TRIGGER fail_stamp BEFORE INSERT ON tape_projection_sources WHEN NEW.tape_uuid = X'{}' BEGIN SELECT RAISE(ABORT, 'injected stamp projection failure'); END;",
            uuid::Uuid::from_bytes(tape).simple(),
        ));
    }

    /// Compare the selective pass with the untouched baseline under the same dispatch lease.
    fn assert_oracle(&self, step: &str) {
        let rt = self.runtime();
        rt.catch_up().unwrap();
        let selected = self.index().normalized_authority_dump();
        {
            let _dispatch = rt.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
            rt.catch_up_oracle_locked().unwrap();
        }
        assert_eq!(
            selected,
            self.index().normalized_authority_dump(),
            "oracle after {step}"
        );
    }
}

/// Every successful selective catch-up agrees with a subsequent full baseline pass.
#[test]
fn selective_catch_up_matches_full_oracle() {
    let f = Fixture::new();
    let rt = f.runtime();
    f.provision(X);
    f.assert_oracle("provision X");
    let old = f.native(X);
    rt.refresh_native(&old).unwrap();
    f.assert_oracle("publish and refresh X");
    rt.project_native(&mut f.index(), &old, ApplyPath::Live)
        .unwrap();
    f.assert_oracle("project X");
    let advanced = f.advance(X);
    rt.refresh_native(&advanced).unwrap();
    f.assert_oracle("advance and refresh X");
    rt.project_native(&mut f.index(), &old, ApplyPath::Live)
        .unwrap();
    f.assert_oracle("project older snapshot");
    f.provision(X);
    f.assert_oracle("duplicate");
    rt.append(|a, v| {
        diagnostic(a, v)?;
        Err(AuthorityError::Store(StoreError::Fold(
            lib::FoldError::HistoryUnavailable,
        )))
    })
    .unwrap_err();
    f.assert_oracle("R6 write");
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = rt.append(|a, v| {
            diagnostic(a, v)?;
            panic!("injected differential write panic");
        });
    }));
    assert!(unwind.is_err());
    f.assert_oracle("panic in write");
    let y = f.native(Y);
    rt.refresh_native(&y).unwrap();
    f.fail(Y);
    f.assert_oracle("Y deferrably failing");
    assert!(rt.projection_lag(Y).is_some());
    f.sql("DROP TRIGGER fail_stamp;");
    f.assert_oracle("Y recovered");
    assert!(rt.projection_lag(Y).is_none());
}

/// Removing a lag on source refusal must leave a full catch-up obligation.
#[test]
fn replaced_source_refusal_invalidates_stamp_after_lag_removal() {
    let f = Fixture::new();
    let rt = f.runtime();
    let old = f.native(X);
    rt.refresh_native(&old).unwrap();
    f.fail(X);
    let replacement = tempfile::tempdir().unwrap();
    let authority = crate::TapeAuthority::open_fixture(replacement.path(), X).unwrap();
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
    let snapshot = owner.freeze().unwrap();
    {
        let _dispatch = rt.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        rt.0.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace_authoritative(snapshot.raw)
            .unwrap();
    }
    rt.catch_up().unwrap();
    assert!(rt.projection_lag(X).is_some());
    assert!(!rt.0.projection_dirty.load(Ordering::Acquire));
    let error = rt
        .project_native(&mut f.index(), &old, ApplyPath::Live)
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("native snapshot belongs to a replaced repair source"));
    assert!(rt.projection_lag(X).is_none());
    assert!(!rt.0.projection_dirty.load(Ordering::Acquire));
    f.sql("DROP TRIGGER fail_stamp;");
    let (_, applies, _) = measured_catch_up(&rt);
    assert_eq!(applies, 1, "T6(a) one Apply after lag removal");
}

/// A failed catch-up cannot restore the stamp after removing its last lag entry.
#[test]
fn nondeferrable_catch_up_leaves_stamp_empty() {
    let f = Fixture::new();
    let rt = f.runtime();
    let initial = f.native(X);
    f.index().project_tape_log(&initial).unwrap();
    let advanced = f.advance(X);
    f.fail(X);
    f.index().project_tape_log(&advanced).unwrap_err();
    rt.catch_up().unwrap();
    assert!(rt.projection_lag(X).is_some());
    let connection = rusqlite::Connection::open(&f.paths.sqlite_path).unwrap();
    let digest: Vec<u8> = connection
        .query_row(
            "SELECT source_digest FROM tape_projection_sources WHERE tape_uuid = ?1",
            [X.as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    f.sql(
        "DROP TRIGGER fail_stamp; UPDATE tape_projection_sources SET source_digest = zeroblob(32);",
    );
    let error = rt.catch_up().unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::Projection(ApplyError::Catalog(StateError::IndexCorrupt(_)))
    ));
    assert!(rt.projection_lag(X).is_none());
    connection
        .execute(
            "UPDATE tape_projection_sources SET source_digest = ?1 WHERE tape_uuid = ?2",
            rusqlite::params![digest, X.as_slice()],
        )
        .unwrap();
    let (_, applies, _) = measured_catch_up(&rt);
    assert_eq!(applies, 1, "T6(b) one Apply after failed catch-up");
    assert_eq!(
        f.index().projected_through_sequence(X).unwrap(),
        advanced.tape_summary().last_closed_sequence
    );
}
