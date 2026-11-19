//! Tape hash continuations and projection markers preserve the full replay result.
use super::*;
use crate::authority::TapeAuthorityView;
use crate::tape_log::{authority::AuthoritySnapshot, CommitCursor, SessionEndReason};

const X: lib::Id = [0x61; 16];

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
            .open_session(
                id,
                "2026-11-12T00:00:00Z".into(),
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
                "2026-11-12T00:00:01Z".into(),
                SessionEndReason::CleanClose,
                None,
                std::iter::empty(),
            )
            .unwrap();
        owner.freeze().unwrap()
    }

    fn capture(&self, id: lib::Id) -> AuthoritySnapshot {
        crate::TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            id,
            &self.paths.sqlite_path,
        )
        .unwrap()
        .acquire_exclusive()
        .unwrap()
        .freeze()
        .unwrap()
    }
}

/// T1: cached, carried and older bounded snapshots agree with a byte-zero oracle.
#[test]
fn prefix_hash_matches_oracle_at_all_ends() {
    // Isolate the extension pass's process-wide counts, as T3 does below.
    const TEST: &str = "library_runtime::prefix_hash_tests::prefix_hash_matches_oracle_at_all_ends";
    const COMPLETE: &str = "LLR1B_PREFIX_EXTENSION_COMPLETE";
    if std::env::var("LLR1B_PREFIX_CHILD").as_deref() != Ok(TEST) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
            .env("LLR1B_PREFIX_CHILD", TEST)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        print!("{stdout}");
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success(),
            "T1 child failed: {}",
            output.status
        );
        assert!(stdout.contains(COMPLETE), "T1 completion marker absent");
        return;
    }
    let f = Fixture::new();
    let mut latest = f.native(X);
    let mut ends = vec![latest.raw.summary().closed_end];
    f.runtime().refresh_native(&latest).unwrap();
    for _ in 0..3 {
        latest
            .raw
            .projection_prefix_digest(latest.raw.summary().closed_end)
            .unwrap();
        latest = f.advance(X);
        ends.push(latest.raw.summary().closed_end);
        f.runtime().refresh_native(&latest).unwrap();
    }
    for sequence in 1..=latest.raw.summary().last_closed_sequence + 1 {
        ends.push(latest.raw.before(sequence).unwrap().summary().closed_end);
    }
    let rt = f.runtime();
    let view = rt.0.view.lock().unwrap().snapshot(X).unwrap().clone();
    let refreshed = f.capture(X);
    rt.refresh_native(&refreshed).unwrap();
    let older = refreshed
        .raw
        .before(refreshed.raw.summary().last_closed_sequence)
        .unwrap();
    assert!(older.summary().closed_end < refreshed.raw.summary().closed_end);
    for (label, snapshot) in [
        ("view", &view),
        ("refreshed", &refreshed.raw),
        ("before", &older),
    ] {
        // Populate the shared cell past the older snapshot's own bound first.
        refreshed
            .raw
            .projection_prefix_digest(refreshed.raw.summary().closed_end)
            .unwrap();
        snapshot
            .projection_prefix_digest(snapshot.summary().closed_end)
            .unwrap();
        let mut requests = vec![0];
        for &end in &ends {
            if end <= snapshot.summary().closed_end {
                requests.extend([end - 1, end]);
            }
        }
        requests.extend([
            snapshot.summary().closed_end - 1,
            snapshot.summary().closed_end,
        ]);
        requests.sort_unstable();
        requests.dedup();
        for end in requests
            .iter()
            .chain(requests.iter().rev())
            .chain(requests.iter())
            .copied()
        {
            assert_eq!(
                snapshot.projection_prefix_digest(end).unwrap(),
                snapshot.projection_prefix_digest_oracle(end).unwrap(),
                "T1 {label} end {end}"
            );
        }
        let beyond = snapshot.summary().closed_end + 1;
        assert!(
            snapshot.projection_prefix_digest(beyond).is_err(),
            "T1 bound {label}"
        );
        assert!(snapshot.projection_prefix_digest_oracle(beyond).is_err());
    }
    let kept_end = refreshed.raw.summary().closed_end;
    assert!(
        older.projection_prefix_digest(kept_end).is_err(),
        "T1 before refuses kept end"
    );
    assert!(older.projection_prefix_digest_oracle(kept_end).is_err());

    // Count only the ascending extension pass, excluding the oracle's reads.
    let ascending = f.capture(X);
    let header_end = ascending.raw.before(1).unwrap().summary().closed_end;
    ends.retain(|&end| end > header_end);
    ends.sort_unstable();
    ends.dedup();
    assert!(ends.len() >= 3, "T1 extension pass has at least three ends");
    let expected: Vec<_> = ends
        .iter()
        .map(|&end| ascending.raw.projection_prefix_digest_oracle(end).unwrap())
        .collect();
    let count = || -> u64 {
        crate::full_pass::counts()
            .iter()
            .filter(|(kind, file, _)| {
                *kind == crate::full_pass::PassKind::TapePrefixHash && *file == file!()
            })
            .map(|(_, _, count)| count)
            .sum()
    };
    let before = count();
    for (&end, digest) in ends.iter().zip(expected) {
        assert_eq!(
            ascending.raw.projection_prefix_digest(end).unwrap(),
            digest,
            "T1 ascending end {end}"
        );
    }
    let delta = count() - before;
    println!("LLR1B T1 ascending extension pass: TapePrefixHash={delta}");
    assert_eq!(delta, 1, "T1 ascending extension pass count");
    println!("{COMPLETE}");
}

/// T6: a tape log shortened after capture refuses without keeping a full-end hash.
#[test]
fn short_read_refuses_and_keeps_no_state() {
    let f = Fixture::new();
    let snapshot = f.native(X);
    let closed_end = snapshot.raw.summary().closed_end;
    let shorter_end = closed_end - 1;
    let store = crate::tape_log::store::Store::open(
        f.paths.journal_dir.join("tapes"),
        X,
        &f.paths.sqlite_path,
    )
    .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(store.path())
        .unwrap()
        .set_len(shorter_end)
        .unwrap();

    assert!(
        matches!(
            snapshot.raw.projection_prefix_digest(closed_end),
            Err(crate::tape_log::store::StoreError::Refused(message))
                if message.contains("shorter than its digest")
        ),
        "T6 short read must refuse with shorter than its digest"
    );
    assert!(
        matches!(
            snapshot.raw.projection_prefix_digest_oracle(closed_end),
            Err(crate::tape_log::store::StoreError::Refused(message))
                if message == "projection source is shorter than its digest"
        ),
        "byte-zero oracle must refuse a short pinned source"
    );
    assert_eq!(
        snapshot.raw.projection_prefix_digest(shorter_end).unwrap(),
        snapshot
            .raw
            .projection_prefix_digest_oracle(shorter_end)
            .unwrap(),
        "T6 shorter end matches oracle after refusal"
    );
}

impl Fixture {
    fn index(&self) -> crate::CatalogIndex {
        self.state.as_ref().unwrap().write_handle()
    }

    fn project(&self, snapshot: &AuthoritySnapshot) {
        self.runtime()
            .project_native(&mut self.index(), snapshot, ApplyPath::Live)
            .unwrap();
    }

    fn reopen(&mut self) {
        drop(self.state.take());
        self.state = Some(
            crate::StateHandle::open_with_config(self.paths.clone(), self.config.clone()).unwrap(),
        );
    }

    fn counts(&self) -> (u64, u64, u64) {
        use crate::full_pass::{counts, PassKind};
        let counts = counts();
        let at = |suffix: &str| {
            counts
                .iter()
                .filter(|(kind, file, _)| {
                    *kind == PassKind::TapePrefixHash && file.ends_with(suffix)
                })
                .map(|(_, _, count)| count)
                .sum()
        };
        (
            at("projector/tape_log_projection.rs"),
            at("library_log/native_view.rs"),
            self.index().fixture_tape_projection_skips().unwrap(),
        )
    }

    fn assert_delta(&self, before: (u64, u64, u64), expected: (u64, u64, u64), step: &str) {
        let after = self.counts();
        let delta = (after.0 - before.0, after.1 - before.1, after.2 - before.2);
        println!("LLR1B T3 {step}: P={} N={} S={}", delta.0, delta.1, delta.2);
        assert_eq!(delta, expected, "T3 {step} (P,N,S)");
    }
}

/// T3 runs alone so the process-wide full-pass counts cannot include another test.
#[test]
fn prefix_hash_and_skip_counts() {
    const TEST: &str = "library_runtime::prefix_hash_tests::prefix_hash_and_skip_counts";
    const COMPLETE: &str = "LLR1B_PREFIX_COUNTS_COMPLETE";
    if std::env::var("LLR1B_PREFIX_CHILD").as_deref() != Ok(TEST) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--test-threads=1", "--nocapture"])
            .env("LLR1B_PREFIX_CHILD", TEST)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        print!("{stdout}");
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(
            output.status.success(),
            "T3 child failed: {}",
            output.status
        );
        assert!(stdout.contains(COMPLETE), "T3 completion marker absent");
        return;
    }
    let mut f = Fixture::new();
    let x = f.native(X);
    let before = f.counts();
    f.project(&x);
    f.assert_delta(before, (1, 0, 0), "(1) published");
    let before = f.counts();
    f.project(&f.capture(X));
    f.assert_delta(before, (0, 0, 1), "(2) unchanged");
    let before = f.counts();
    let advanced = f.advance(X);
    f.project(&advanced);
    f.assert_delta(before, (0, 0, 0), "(3) advanced");

    // The next lease replaces the torn suffix by rename before adding a group.
    use std::io::Write;
    let store = crate::tape_log::store::Store::open(
        f.paths.journal_dir.join("tapes"),
        X,
        &f.paths.sqlite_path,
    )
    .unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.path())
        .unwrap()
        .write_all(&[0; 100])
        .unwrap();
    let before = f.counts();
    let replaced = f.advance(X);
    assert_ne!(
        advanced.raw.file_identity().unwrap(),
        replaced.raw.file_identity().unwrap()
    );
    f.project(&replaced);
    f.assert_delta(before, (1, 0, 0), "(4) renamed");

    let retained = crate::projector::open(&f.paths.sqlite_path)
        .unwrap()
        .write_handle();
    let before = f.counts();
    f.reopen();
    f.assert_delta(before, (0, 1, 0), "(5) reopened with retained core");
    let before = f.counts();
    f.project(&f.capture(X));
    f.assert_delta(before, (0, 0, 1), "(5) unchanged after reopen");
    drop(retained);
    println!("{COMPLETE}");
}

const Y: lib::Id = [0x62; 16];

impl Fixture {
    fn sql(&self, sql: &str) {
        rusqlite::Connection::open(&self.paths.sqlite_path)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
    }

    fn assert_oracle(&self, step: &str) {
        let runtime = self.runtime();
        let _dispatch = runtime.0.dispatch.lock().unwrap_or_else(|p| p.into_inner());
        let before = self.index().normalized_authority_dump();
        let snapshots: Vec<_> = runtime.0.view.lock().unwrap().snapshots().collect();
        for raw in snapshots {
            self.index()
                .project_tape_log_bounded_oracle(&AuthoritySnapshot::new(raw).unwrap())
                .unwrap();
        }
        assert_eq!(
            before,
            self.index().normalized_authority_dump(),
            "T2 {step}"
        );
    }

    fn summary_counters(&self) -> (u64, u64, Option<u64>) {
        let tape = self.index().get_tape(&X).unwrap().unwrap();
        (
            tape.highest_protected_ordinal,
            tape.total_committed_ordinals,
            tape.last_committed_tape_file,
        )
    }
}

/// T2: the optimized path matches full byte-zero projection, including a rolled
/// back rebuild whose first tape succeeded only inside the outer transaction.
#[test]
fn projection_matches_full_oracle_after_rebuild_failure() {
    let f = Fixture::new();
    let x = f.native(X);
    f.project(&x);
    f.assert_oracle("(a) published");
    let before = f.index().fixture_tape_projection_skips().unwrap();
    f.project(&f.capture(X));
    assert_eq!(
        f.index().fixture_tape_projection_skips().unwrap() - before,
        1,
        "T2 (b) skip"
    );
    f.assert_oracle("(b) unchanged");
    f.project(&f.advance(X));
    f.assert_oracle("(c) advanced");
    f.index()
        .reconcile_tape_files_projection(X, &[], 7, 9)
        .unwrap();
    f.runtime().catch_up().unwrap();
    f.assert_oracle("(d) reconcile and catch-up");
    f.project(&f.native(Y));
    f.index()
        .reconcile_tape_files_projection(X, &[], 7, 9)
        .unwrap();
    f.sql(&format!("CREATE TRIGGER fail_llr1b BEFORE INSERT ON tape_projection_sources WHEN NEW.tape_uuid = X'{}' BEGIN SELECT RAISE(ABORT, 'injected LLR-1b Y projection failure'); END;", uuid::Uuid::from_bytes(Y).simple()));
    let error = f.runtime().rebuild_projection(&mut f.index()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected LLR-1b Y projection failure"),
        "{error}"
    );
    f.sql("DROP TRIGGER fail_llr1b;");
    f.runtime().catch_up().unwrap();
    f.assert_oracle("(e) catch-up after rolled-back rebuild");
}

/// T4: each direct writer invalidates completion before even a refused call,
/// while preserving the hash continuation of its unchanged native bytes.
#[test]
fn direct_funnels_forget_projection_markers() {
    let f = Fixture::new();
    f.native(X);
    f.project(&f.advance(X));
    let expected = f.summary_counters();
    f.index()
        .reconcile_tape_files_projection(X, &[], 7, 9)
        .unwrap();
    let before = f.index().fixture_tape_projection_skips().unwrap();
    f.project(&f.capture(X));
    assert_eq!(
        f.index().fixture_tape_projection_skips().unwrap(),
        before,
        "T4 (a) not a skip"
    );
    assert_eq!(
        f.summary_counters(),
        expected,
        "T4 (a) restores three counters"
    );
    f.project(&f.capture(X));
    assert_eq!(
        f.index().fixture_tape_projection_skips().unwrap(),
        before + 1,
        "T4 (a) next is a skip"
    );
    for outcome in [
        crate::TerminalFinalizationOutcome::InProgress,
        crate::TerminalFinalizationOutcome::Finalized,
    ] {
        let result =
            f.index()
                .project_terminal_finalization(crate::TerminalFinalizationProjectionInput {
                    tape_uuid: X,
                    trigger: crate::TerminalFinalizationTrigger::OperatorCloseOut,
                    operation_id: Some(uuid::Uuid::from_u128(0xFA11)),
                    progress: crate::TerminalFinalizationProgress::BeforeReplicaA,
                    edition_digest: [0xED; 32],
                    layout_digest: [0x1A; 32],
                    outcome,
                    updated_at_utc: Some("2026-08-09T00:00:00Z".into()),
                });
        if outcome == crate::TerminalFinalizationOutcome::InProgress {
            result.unwrap();
        } else {
            result.unwrap_err();
        }
        let before = f.index().fixture_tape_projection_skips().unwrap();
        f.project(&f.capture(X));
        assert_eq!(
            f.index().fixture_tape_projection_skips().unwrap(),
            before,
            "T4 (b) not a skip after {outcome:?}"
        );
    }
}
