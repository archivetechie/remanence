//! N-2: real tape apply failures stay local, and only a sufficiently new
//! successful apply at A clears lag. Fixtures retain typed library authority.
use super::*;
use crate::authority::TapeAuthorityView;
use crate::tape_log::{authority::AuthoritySnapshot, CommitCursor, SessionEndReason};

const X: lib::Id = [0x61; 16];
const Y: lib::Id = [0x62; 16];
const INJECTED: &str = "injected N-2 tape projection failure";

struct Fixture {
    state: Option<crate::StateHandle>,
    paths: StatePaths,
    config: crate::RemConfig,
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
            config,
            _root: root,
        }
    }

    fn runtime(&self) -> LibraryRuntime {
        self.state.as_ref().unwrap().library_runtime()
    }

    fn index(&self) -> crate::CatalogIndex {
        self.state.as_ref().unwrap().write_handle()
    }

    fn sql(&self, sql: &str) {
        rusqlite::Connection::open(&self.paths.sqlite_path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    fn provision(&self, id: lib::Id) {
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
            .unwrap();
    }

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
            .open_session(id, "2026-11-12T00:00:00Z".into())
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

    fn fail(&self, sequence: Option<u64>) {
        let sequence = sequence
            .map(|n| format!(" AND NEW.received_sequence = X'{n:016x}'"))
            .unwrap_or_default();
        self.sql(&format!(
            "CREATE TRIGGER fail_n2 BEFORE INSERT ON tape_projection_sources WHEN NEW.tape_uuid = X'{}'{sequence} BEGIN SELECT RAISE(ABORT, '{INJECTED}'); END;",
            uuid::Uuid::from_bytes(X).simple(),
        ));
    }

    fn reopen(&mut self) {
        drop(self.state.take());
        self.state = Some(
            crate::StateHandle::open_with_config(self.paths.clone(), self.config.clone()).unwrap(),
        );
    }
}

#[test]
fn deferrable_apply_records_lag_and_next_append_and_motion_recover() {
    let f = Fixture::new();
    let snapshot = f.native(X);
    f.fail(None);
    let runtime = f.runtime();
    let error = runtime
        .project_native(&mut f.index(), &snapshot, ApplyPath::Live)
        .unwrap_err();
    assert!(matches!(
        &error,
        ApplyError::Catalog(StateError::Index { .. })
    ));
    assert!(error.to_string().contains(INJECTED));
    assert!(runtime.projection_lag(X).unwrap().contains(INJECTED));
    assert!(runtime.0.projection_dirty.load(Ordering::Acquire));
    f.provision(Y); // A real library append must get past the failing tape in B.
    assert!(!runtime.0.projection_dirty.load(Ordering::Acquire));
    assert!(runtime.projection_lag(Y).is_none());
    runtime
        .project_native(&mut f.index(), &snapshot, ApplyPath::Live)
        .unwrap_err();
    assert!(runtime.0.projection_dirty.load(Ordering::Acquire));
    runtime.ensure_motion_allowed().unwrap();
    assert!(!runtime.0.projection_dirty.load(Ordering::Acquire));
    assert!(runtime.projection_lag(X).is_some());
    f.sql("DROP TRIGGER fail_n2;");
    runtime
        .project_native(&mut f.index(), &snapshot, ApplyPath::Live)
        .unwrap();
    assert!(runtime.projection_lag(X).is_none());
}

#[test]
fn catch_up_and_open_defer_one_tape_then_clear_on_success() {
    let mut f = Fixture::new();
    let x = f.native(X);
    let y = f.native(Y);
    f.runtime().refresh_native(&x).unwrap();
    f.runtime().refresh_native(&y).unwrap();
    f.fail(None);
    f.runtime().catch_up().unwrap();
    assert!(f.runtime().projection_lag(X).unwrap().contains(INJECTED));
    assert!(f.runtime().projection_lag(Y).is_none());
    assert_eq!(
        f.index().projected_through_sequence(Y).unwrap(),
        y.tape_summary().last_closed_sequence
    );
    assert!(!f.runtime().0.projection_dirty.load(Ordering::Acquire));
    f.reopen();
    assert!(f.runtime().projection_lag(X).unwrap().contains(INJECTED));
    assert!(f.runtime().projection_lag(Y).is_none());
    f.sql("DROP TRIGGER fail_n2;");
    f.runtime().catch_up().unwrap();
    assert!(f.runtime().projection_lag(X).is_none());
    assert_eq!(
        f.index().projected_through_sequence(X).unwrap(),
        x.tape_summary().last_closed_sequence
    );
}

#[test]
fn nondeferrable_apply_and_worker_failure_remove_prior_lag() {
    for worker in [false, true] {
        let f = Fixture::new();
        let s = f.native(X);
        f.index().project_tape_log(&s).unwrap();
        let n = f.advance(X);
        f.fail(None);
        f.index().project_tape_log(&n).unwrap_err();
        f.runtime().catch_up().unwrap();
        assert!(f.runtime().projection_lag(X).is_some());
        f.sql("DROP TRIGGER fail_n2; UPDATE tape_projection_sources SET source_digest = zeroblob(32);");
        if worker {
            let error = f.runtime().catch_up().unwrap_err();
            assert!(matches!(
                error,
                RuntimeError::Projection(ApplyError::Catalog(StateError::IndexCorrupt(_)))
            ));
        } else {
            let error = f
                .runtime()
                .project_native(&mut f.index(), &n, ApplyPath::Live)
                .unwrap_err();
            assert!(matches!(
                error,
                ApplyError::Catalog(StateError::IndexCorrupt(_))
            ));
            assert!(f.runtime().0.projection_dirty.load(Ordering::Acquire));
        }
        assert!(f.runtime().projection_lag(X).is_none());
    }
}

#[test]
fn preapply_source_refusal_removes_prior_lag_without_applying() {
    let f = Fixture::new();
    let old = f.native(X);
    f.fail(None);
    f.index().project_tape_log(&old).unwrap_err();
    let runtime = f.runtime();
    assert!(runtime.projection_lag(X).is_some());
    // Exercise the existing replaced-source refusal before A's apply.
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
        let _dispatch = runtime.0.dispatch.lock().unwrap();
        runtime
            .0
            .view
            .lock()
            .unwrap()
            .replace_authoritative(snapshot.raw)
            .unwrap();
    }
    let error = runtime
        .project_native(&mut f.index(), &old, ApplyPath::Live)
        .unwrap_err();
    assert!(matches!(
        &error,
        ApplyError::Catalog(StateError::JournalReplayFailed(_))
    ));
    assert!(error
        .to_string()
        .contains("native snapshot belongs to a replaced repair source"));
    assert!(runtime.projection_lag(X).is_none());
    assert!(runtime.0.projection_dirty.load(Ordering::Acquire));
    assert_eq!(f.index().projected_through_sequence(X).unwrap(), 0);
}

#[test]
fn t6_old_success_preserves_newer_lag_and_retry_or_open_recovers() {
    for reopen in [false, true] {
        let mut f = Fixture::new();
        let s = f.native(X);
        f.index().project_tape_log(&s).unwrap();
        let n = f.advance(X);
        let old_sequence = s.tape_summary().last_closed_sequence;
        let new_sequence = n.tape_summary().last_closed_sequence;
        assert!(new_sequence > old_sequence);
        let runtime = f.runtime();
        runtime.refresh_native(&n).unwrap();
        assert_eq!(runtime.0.ack_progress.lock().unwrap()[&X].0, old_sequence);
        f.fail(Some(new_sequence));
        // Arrange a dirty dispatch without advancing X's validation progress.
        // The nested catch-up, not this setup, must produce the newer lag.
        runtime.0.projection_dirty.store(true, Ordering::Release);
        let marks = runtime
            .project_native(&mut f.index(), &s, ApplyPath::Live)
            .unwrap();
        assert_eq!(marks.applied, old_sequence);
        assert_eq!(
            f.index().projected_through_sequence(X).unwrap(),
            old_sequence
        );
        assert_eq!(runtime.0.ack_progress.lock().unwrap()[&X].0, new_sequence);
        assert!(runtime
            .projection_lag(X)
            .expect("older success must retain newer lag")
            .contains(INJECTED));
        assert_eq!(
            runtime.0.projection_lag.lock().unwrap()[&X].last_closed_sequence,
            new_sequence
        );
        drop(runtime);
        if reopen {
            f.reopen();
            assert!(f.runtime().projection_lag(X).unwrap().contains(INJECTED));
            assert_eq!(
                f.runtime().0.projection_lag.lock().unwrap()[&X].last_closed_sequence,
                new_sequence
            );
        }
        f.sql("DROP TRIGGER fail_n2;");
        f.runtime().catch_up().unwrap();
        assert!(f.runtime().projection_lag(X).is_none());
        assert_eq!(
            f.index().projected_through_sequence(X).unwrap(),
            new_sequence
        );
    }
}

#[test]
fn projection_deferral_classifier_preserves_semantic_failures() {
    for error in [
        StateError::Index {
            context: "projection".into(),
            source: rusqlite::Error::InvalidQuery,
        },
        StateError::io("projection", std::io::Error::other("offline")),
        StateError::DiskFull("projection".into()),
    ] {
        assert!(error.is_deferrable_projection_failure());
    }
    for error in [
        StateError::IndexCorrupt("digest".into()),
        StateError::JournalReplayFailed("wrapped error".into()),
        StateError::IndexMigrationFailed("schema".into()),
        StateError::CatalogReentered,
        StateError::from(crate::tape_log::store::StoreError::Header),
    ] {
        assert!(!error.is_deferrable_projection_failure());
    }
}
