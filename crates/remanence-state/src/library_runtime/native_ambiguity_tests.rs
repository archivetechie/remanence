//! Operator recovery regressions using the real lost-fence fixture and durable-file faults.
use super::*;
use crate::ambiguity_recovery::{self as recovery, LIBRARY_LOG_REPAIR_DEFERRED};
use std::{
    fs,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

struct Offline {
    paths: StatePaths,
    uuid: lib::Id,
    _root: tempfile::TempDir,
}
impl Offline {
    fn from_fixture(f: Fixture) -> Self {
        let Fixture {
            runtime,
            paths,
            lease,
            uuid,
            _root,
        } = f;
        drop(runtime);
        drop(lease);
        fs::write(&paths.config_path, format!(
            "[daemon]\ndefault_idle_timeout_seconds = 1800\nread_only = false\nstate_dir = {:?}\n[journal]\ndir = {:?}\nrequire_trusted_volume = false\n[audit]\ndir = {:?}\nfsync = true\n[index]\nsqlite_path = {:?}\n[cache]\ntape_catalog_dir = {:?}\n",
            paths.state_dir, paths.journal_dir, paths.audit_dir, paths.sqlite_path, paths.tape_cache_dir
        )).unwrap();
        Self { paths, uuid, _root }
    }
    fn lost() -> Self {
        let (f, _) = Fixture::failed();
        f.lose(T);
        Self::from_fixture(f)
    }
    fn marker(&self) -> PathBuf {
        self.paths.audit_dir.join("library-log.ambiguous")
    }
    fn tape_marker(&self) -> PathBuf {
        self.paths
            .journal_dir
            .join("tapes")
            .join(format!("{}.ambiguous", uuid::Uuid::from_bytes(T).simple()))
    }
    fn segment(&self) -> PathBuf {
        let mut files: Vec<_> = fs::read_dir(&self.paths.audit_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "remlib"))
            .collect();
        files.sort();
        files.pop().unwrap()
    }
    fn replay(&self) -> lib::store::Replay {
        let view = NativeTapeView::probe(
            &self.paths.journal_dir.join("tapes"),
            &self.paths.sqlite_path,
        )
        .unwrap();
        lib::store::replay(&self.paths.audit_dir, self.uuid, &view).unwrap()
    }
    fn recover(&self) -> Result<serde_json::Value, StateError> {
        recovery::recover_with_space_probe(&self.paths, true, |_| Ok(u64::MAX))
    }
    fn reopen(&self) -> Result<LibraryRuntime, RuntimeError> {
        let lease = Arc::new(StateLockGuard::acquire(&self.paths.state_dir).unwrap());
        LibraryRuntime::open(&self.paths, true, lease)
    }
    fn inspect_unchanged(&self) -> serde_json::Value {
        let before = digest(self._root.path());
        let result = recovery::inspect(&self.paths.config_path).unwrap();
        assert_eq!(
            digest(self._root.path()),
            before,
            "inspection changed the tree"
        );
        result
    }
}

fn digest(root: &Path) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    fn visit(root: &Path, dir: &Path, hash: &mut Sha256) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.file_name().is_some_and(|n| n == "state.lock")
                || path.extension().is_some_and(|e| e == "lock")
            {
                continue;
            }
            hash.update(
                path.strip_prefix(root)
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes(),
            );
            if path.is_dir() {
                visit(root, &path, hash);
            } else {
                hash.update(fs::read(path).unwrap());
            }
        }
    }
    let mut hash = Sha256::new();
    visit(root, root, &mut hash);
    hash.finalize().to_vec()
}

fn diagnostic() -> lib::Diagnostic {
    lib::Diagnostic {
        event: lib::DiagnosticEvent::RecoveryEvent,
        subject_kind: "test".into(),
        subject_id: None,
        detail: lib::Bounded(vec![]),
    }
}

#[test]
fn fx1_t1a_lost_fence_recovers_and_restarts() {
    let f = Offline::lost();
    assert!(f.reopen().is_err());
    assert!(f.reopen().is_err());
    f.inspect_unchanged();
    let inode = fs::metadata(f.segment()).unwrap().ino();
    let report = f.recover().unwrap();
    assert!(!f.marker().exists());
    assert_ne!(fs::metadata(f.segment()).unwrap().ino(), inode);
    assert!(
        !crate::CatalogReader::open(&f.paths.sqlite_path)
            .unwrap()
            .tape_io_admission_conflicts(&T, None)
            .unwrap()
            .is_empty(),
        "recovery did not raise the owed fence before any reopen"
    );
    let recovered_tree = digest(f._root.path());
    f.recover().unwrap();
    assert_eq!(
        digest(f._root.path()),
        recovered_tree,
        "a converged rerun must not append or rewrite"
    );
    let runtime = f.reopen().unwrap();
    assert!(
        !runtime
            .reader()
            .unwrap()
            .tape_io_admission_conflicts(&T, None)
            .unwrap()
            .is_empty(),
        "owed fence was not raised"
    );
    assert_eq!(report["reconciliation"][0]["outcome"], "raised");
    drop(runtime);
    let runtime = f.reopen().unwrap();
    runtime
        .append(|a, v| {
            a.diagnostic(
                diagnostic(),
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
}

#[test]
fn fx1_t2_torn_tail_keeps_exact_prefix() {
    let f = Offline::lost();
    let segment = f.segment();
    let kept = fs::read(&segment).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&segment)
        .unwrap()
        .write_all(&kept[84..87])
        .unwrap();
    f.inspect_unchanged();
    f.recover().unwrap();
    let bytes = fs::read(segment).unwrap();
    assert_eq!(&bytes[..kept.len()], kept);
    assert!(f.replay().refusal.is_none());
    assert_eq!(
        f.replay().segments.last().unwrap().tail,
        crate::log_frame::Tail::Complete
    );
}

#[test]
fn fx1_t3_landed_sync_republishes_complete_segment() {
    let (f, _) = Fixture::failed();
    let registration = register(
        f.paths.audit_dir.clone(),
        Arc::new(durability::FaultDirectory(Arc::new(
            durability::Injection {
                stage: AtomicU8::new(2),
                after: false,
            },
        ))),
    );
    assert!(f
        .rt()
        .append(|a, v| a.diagnostic(
            diagnostic(),
            lib::AuditActor::System,
            lib::SourceLayer::Layer4,
            v
        ))
        .is_err());
    drop(registration);
    let f = Offline::from_fixture(f);
    let before = f.replay();
    let kept = fs::read(f.segment()).unwrap();
    let inode = fs::metadata(f.segment()).unwrap().ino();
    f.inspect_unchanged();
    let report = f.recover().unwrap();
    assert_ne!(
        fs::metadata(f.segment()).unwrap().ino(),
        inode,
        "complete ambiguous segment must be republished"
    );
    assert_eq!(&fs::read(f.segment()).unwrap()[..kept.len()], kept);
    assert_eq!(
        report["retained_library_end"]["last_sequence"],
        before.last_sequence
    );
    let records = f.reopen().unwrap().test_source_records();
    let recovered = records
        .iter()
        .filter_map(|r| match r.body_view() {
            lib::BodyView::Diagnostic(d) if d.subject_kind.as_str() == "ambiguity-recovery" => {
                Some(d)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(recovered.len(), 1);
    assert!(recovered[0]
        .detail
        .0
        .iter()
        .any(|e| e.0.as_str() == "retained_sequence"
            && e.1 == lib::DiagnosticValue::Unsigned(before.last_sequence)));
}

#[test]
fn fx1_t3b_failed_first_append_without_segment() {
    let f = Fixture::new();
    let registration = register(
        f.paths.audit_dir.clone(),
        Arc::new(durability::FaultDirectory(Arc::new(
            durability::Injection {
                stage: AtomicU8::new(1),
                after: false,
            },
        ))),
    );
    assert!(f
        .rt()
        .append(|a, v| a.diagnostic(
            diagnostic(),
            lib::AuditActor::System,
            lib::SourceLayer::Layer4,
            v
        ))
        .is_err());
    drop(registration);
    let f = Offline::from_fixture(f);
    assert!(f.replay().segments.is_empty());
    f.inspect_unchanged();
    let report = f.recover().unwrap();
    assert_eq!(report["retained_library_end"]["last_sequence"], 0);
    assert!(!f.marker().exists());
    f.reopen().unwrap();
}

#[test]
fn fx1_t4_tape_before_library_raises_owed_fence() {
    let f = Offline::lost();
    fs::write(f.tape_marker(), b"").unwrap();
    f.inspect_unchanged();
    let result = f.recover();
    assert!(
        result.is_ok(),
        "tape must be cleared before probing library: {result:?}"
    );
    assert!(!f.tape_marker().exists());
    assert!(!f.marker().exists());
    assert_eq!(result.unwrap()["reconciliation"][0]["outcome"], "raised");
    assert!(!f
        .reopen()
        .unwrap()
        .reader()
        .unwrap()
        .tape_io_admission_conflicts(&T, None)
        .unwrap()
        .is_empty());
}

#[test]
fn fx1_t4b_tape_only_does_not_republish_library() {
    let (f, _) = Fixture::failed();
    let f = Offline::from_fixture(f);
    fs::write(f.tape_marker(), b"").unwrap();
    let inode = fs::metadata(f.segment()).unwrap().ino();
    f.inspect_unchanged();
    f.recover().unwrap();
    assert_eq!(
        fs::metadata(f.segment()).unwrap().ino(),
        inode,
        "unmarked library must not be republished"
    );
}

#[test]
fn fx1_t5_live_state_lease_refuses_unchanged() {
    let f = Offline::lost();
    let _lease = StateLockGuard::acquire(&f.paths.state_dir).unwrap();
    let before = digest(f._root.path());
    let report = f.inspect_unchanged();
    assert_eq!(report["daemon_running"], true);
    assert!(matches!(f.recover(), Err(StateError::StateLockHeld(_))));
    assert_eq!(digest(f._root.path()), before);
}

#[test]
fn fx1_t6_space_refuses_before_authority_writes() {
    let f = Offline::lost();
    f.inspect_unchanged();
    let before = digest(f._root.path());
    let error = recovery::recover_with_space_probe(&f.paths, true, |_| Ok(0)).unwrap_err();
    assert!(error.to_string().contains("free space"), "{error}");
    assert_eq!(digest(f._root.path()), before);
}

#[test]
fn fx1_t7_inspect_preserves_unmarked_tail_and_temporary() {
    let (f, _) = Fixture::failed();
    let f = Offline::from_fixture(f);
    let original = fs::read(f.segment()).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(f.segment())
        .unwrap()
        .write_all(&original[84..87])
        .unwrap();
    fs::write(
        f.paths.audit_dir.join(".segment-library-log-lingering"),
        b"evidence",
    )
    .unwrap();
    let report = f.inspect_unchanged();
    assert!(!report["library_replay"].is_null());
    assert!(report["library_replay"]["refusal"].is_null());
    assert_ne!(report["library_replay"]["tail"], "Complete");
}

#[test]
fn fx1_t8_recovery_converges_after_rename_and_reconciliation_cuts() {
    for stage in [3, 4] {
        let f = Offline::lost();
        let fault = register(
            f.paths.audit_dir.clone(),
            Arc::new(durability::FaultDirectory(Arc::new(
                durability::Injection {
                    stage: AtomicU8::new(stage),
                    after: true,
                },
            ))),
        );
        assert!(f.recover().is_err());
        drop(fault);
        f.recover().unwrap();
        f.reopen().unwrap();
    }
    for stage in [1, 2] {
        let f = Offline::lost();
        CUT.set(stage);
        let result = f.recover();
        CUT.set(0);
        assert!(result.is_err());
        assert!(f
            .paths
            .audit_dir
            .join("library-log.fence-reconcile")
            .exists());
        f.recover().unwrap();
        f.reopen().unwrap();
    }
}

#[test]
fn fx1_t9_damage_with_and_without_marker_leaves_tree() {
    for marked in [false, true] {
        for damage in [0, 1, 2] {
            let (f, _) = Fixture::failed();
            let f = Offline::from_fixture(f);
            let segment = f.segment();
            if damage == 2 {
                struct Later;
                impl Clock for Later {
                    fn now(&self) -> u64 {
                        2_000_000_000_000_000_000
                    }
                }
                let view =
                    NativeTapeView::probe(&f.paths.journal_dir.join("tapes"), &f.paths.sqlite_path)
                        .unwrap();
                let lease = Arc::new(StateLockGuard::acquire(&f.paths.state_dir).unwrap());
                let authority = LibraryAuthority::open_bound(
                    &f.paths.audit_dir,
                    f.uuid,
                    Origin {
                        host: "test".into(),
                        pid: 1,
                        build: "test".into(),
                    },
                    Box::new(Later),
                    Box::new(FreshUuids),
                    true,
                    &view,
                    lease,
                )
                .unwrap();
                authority
                    .diagnostic(
                        diagnostic(),
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer4,
                        &view,
                    )
                    .unwrap();
                drop(authority);
                assert_eq!(f.replay().segments.len(), 2);
            }
            let mut bytes = fs::read(&segment).unwrap();
            let at = if damage == 1 { 90 } else { bytes.len() - 1 };
            bytes[at] ^= 1;
            fs::write(&segment, bytes).unwrap();
            if marked {
                fs::write(f.marker(), b"").unwrap();
            }
            let inspection = f.inspect_unchanged();
            assert!(!inspection["library_replay"]["refusal"].is_null());
            let before = digest(f._root.path());
            let error = f.recover().unwrap_err();
            assert!(matches!(error, StateError::JournalReplayFailed(_)));
            assert!(
                error.to_string().contains(LIBRARY_LOG_REPAIR_DEFERRED),
                "{error}"
            );
            assert!(
                error
                    .to_string()
                    .contains(segment.file_name().unwrap().to_str().unwrap()),
                "{error}"
            );
            assert_eq!(digest(f._root.path()), before);
        }
    }
}

#[test]
fn fx1_t10_startup_hint_with_state_lease() {
    let f = Offline::lost();
    let _lease = StateLockGuard::acquire(&f.paths.state_dir).unwrap();
    let hint = recovery::startup_hint(&f.paths).unwrap();
    assert!(hint.contains(f.marker().to_str().unwrap()));
    assert!(hint.contains("restart does not clear"));
    assert!(hint.contains("rem-debug library-log recover --config"));
    assert!(hint.contains("--confirm-clear-ambiguity"));
}

/// Exercise R1 directly so the base failure is the inode assertion itself.
#[test]
fn fx1_r1_complete_ambiguous_segment_requires_new_inode() {
    let (f, _) = Fixture::failed();
    f.lose(T);
    let mut f = f;
    let inode = fs::metadata(OfflineSegment::path(&f.paths)).unwrap().ino();
    f.clear();
    assert_ne!(
        fs::metadata(OfflineSegment::path(&f.paths)).unwrap().ino(),
        inode,
        "complete ambiguous segment must be republished"
    );
}
struct OfflineSegment;
impl OfflineSegment {
    fn path(paths: &StatePaths) -> PathBuf {
        fs::read_dir(&paths.audit_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|e| e == "remlib"))
            .unwrap()
    }
}

/// Fail the first directory sync after the ambiguity marker has been removed.
#[test]
fn fx1_t8_after_marker_removal_converges() {
    use remanence_library::durable_file::{DurableDirectory, DurableFile};
    struct AfterRemoval {
        marker: PathBuf,
        tripped: std::sync::atomic::AtomicBool,
        inner: durability::FaultDirectory,
    }
    impl DurableDirectory for AfterRemoval {
        fn create(&self, p: &Path) -> std::io::Result<Box<dyn DurableFile>> {
            self.inner.create(p)
        }
        fn open(&self, p: &Path) -> std::io::Result<Box<dyn DurableFile>> {
            self.inner.open(p)
        }
        fn rename(&self, a: &Path, b: &Path) -> std::io::Result<()> {
            self.inner.rename(a, b)
        }
        fn rename_noreplace(&self, a: &Path, b: &Path) -> std::io::Result<()> {
            self.inner.rename_noreplace(a, b)
        }
        fn sync(&self, p: &Path) -> std::io::Result<()> {
            if !self.marker.exists() && !self.tripped.swap(true, Ordering::SeqCst) {
                return Err(std::io::Error::other("after marker removal"));
            }
            self.inner.sync(p)
        }
        fn set_len(&self, _: &Path, _: u64) -> std::io::Result<()> {
            panic!("no truncation")
        }
    }
    let f = Offline::lost();
    let cut = Arc::new(AfterRemoval {
        marker: f.marker(),
        tripped: std::sync::atomic::AtomicBool::new(false),
        inner: durability::FaultDirectory(Arc::new(durability::Injection {
            stage: AtomicU8::new(0),
            after: false,
        })),
    });
    let registration = register(f.paths.audit_dir.clone(), cut.clone());
    assert!(f.recover().is_err());
    assert!(cut.tripped.load(Ordering::SeqCst));
    drop(registration);
    f.recover().unwrap();
    f.reopen().unwrap();
}
