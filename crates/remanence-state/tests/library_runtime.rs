//! Production runtime boundaries: manifest identity, shared typed publication,
//! SQL loss replay, and storage-failure motion fencing without audit imports.
use remanence_state::{
    authority_manifest::AuthorityManifest, library_log::*, library_runtime::LibraryRuntime,
    StateLockGuard, StatePaths,
};
use std::{path::Path, sync::Arc};
fn paths(root: &Path) -> StatePaths {
    StatePaths {
        config_path: root.join("config"),
        state_dir: root.join("state"),
        audit_dir: root.join("external/library"),
        journal_dir: root.join("external/native"),
        sqlite_path: root.join("catalog.sqlite"),
        tape_cache_dir: root.join("cache"),
        calibration_dir: root.join("calibration"),
    }
}
fn provision() -> Reprovision {
    Reprovision {
        pool_source: Some(PoolSource::Operator(Unit)),
        operation: None,
        replaced_bot: None,
        tape: TapeRef {
            uuid: [0xa1; 16],
            incarnation: 1,
        },
        predecessor: None,
        mode: ProvisionMode::Initialize,
        label: Some("RUNTIME-L9".into()),
        label_provenance: Some(LabelProvenance::Operator),
        parity_scheme: None,
        medium: MediumSpec {
            family: MediumFamily::Lto(LtoGen::Lto9),
            usable_logical_bytes: 18_000_000_000_000,
            record_size: 262144,
            rerecord: true,
            lifecycle_ops: true,
            source: GenerationSource::Operator,
        },
        pool: Some("runtime".into()),
        endpoint: None,
        adoption: None,
    }
}
#[test]
fn runtime_shares_real_identity_and_rebuilds_fence_after_sql_loss() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    assert_eq!(AuthorityManifest::preflight(&p).unwrap(), None);
    let lease = Arc::new(StateLockGuard::acquire(&p.state_dir).unwrap());
    let runtime = LibraryRuntime::open(&p, true, lease.clone()).unwrap();
    let peer = runtime.clone();
    let id = runtime.manifest().library_uuid();
    runtime
        .append(|a, v| a.reprovision(provision(), AuditActor::System, SourceLayer::Layer4, v))
        .unwrap();
    assert_eq!(
        peer.tape_state([0xa1; 16]).unwrap().assignment_generation,
        1
    );
    let raise = peer
        .append(|a, v| {
            a.fence_raised(
                FenceRaised {
                    detail: Bounded(vec![]),
                    kind: FenceKind::Write,
                    scope: FenceScope::Tape(provision().tape),
                    cause: FenceCause::SessionSuffixContradiction,
                    evidence: None,
                    at: None,
                    operation: None,
                },
                AuditActor::System,
                SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    assert!(raise.synced);
    assert_eq!(
        runtime
            .reader()
            .unwrap()
            .list_active_tape_io_fences()
            .unwrap()
            .len(),
        1
    );
    drop(peer);
    drop(runtime);
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", p.sqlite_path.display()));
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let runtime = LibraryRuntime::open(&p, true, lease.clone()).unwrap();
    assert_eq!(runtime.manifest().library_uuid(), id);
    assert_eq!(
        runtime
            .reader()
            .unwrap()
            .list_active_tape_io_fences()
            .unwrap()
            .len(),
        1
    );
    runtime
        .append(|a, v| {
            a.fence_released(
                FenceReleased {
                    detail: Bounded(vec![]),
                    fence: raise.sequence,
                    evidence: "explicit physical tail proof".into(),
                    forced: false,
                },
                AuditActor::System,
                SourceLayer::Layer4,
                v,
            )
        })
        .unwrap();
    drop(runtime);
    let runtime = LibraryRuntime::open(&p, true, lease).unwrap();
    assert!(runtime
        .reader()
        .unwrap()
        .list_active_tape_io_fences()
        .unwrap()
        .is_empty());
}
#[test]
fn runtime_distinguishes_fold_refusal_from_sticky_store_failure() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let runtime = LibraryRuntime::open(
        &p,
        true,
        Arc::new(StateLockGuard::acquire(&p.state_dir).unwrap()),
    )
    .unwrap();
    assert!(runtime
        .append(|a, v| a.fence_raised(
            FenceRaised {
                detail: Bounded(vec![]),
                kind: FenceKind::Write,
                scope: FenceScope::Tape(provision().tape),
                cause: FenceCause::SessionSuffixContradiction,
                evidence: None,
                at: None,
                operation: None
            },
            AuditActor::System,
            SourceLayer::Layer4,
            v
        ))
        .is_err());
    assert!(!runtime.motion_fenced());
    runtime.ensure_motion_allowed().unwrap();
    let peer = runtime.clone();
    assert!(runtime
        .append(|_, _| Err(AuthorityError::Store(store::StoreError::Io(
            std::io::Error::other("injected authority failure")
        ))))
        .is_err());
    assert!(peer.motion_fenced());
    assert!(peer.ensure_motion_allowed().is_err());
}
#[test]
fn manifest_refuses_unbound_authority_without_changing_any_bytes() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    std::fs::create_dir_all(&p.audit_dir).unwrap();
    let old = p.audit_dir.join("2026-11-01.remaudit");
    std::fs::write(&old, b"old authority, never decoded").unwrap();
    std::fs::write(&p.sqlite_path, b"unknown SQL version or corrupt database").unwrap();
    let error = AuthorityManifest::preflight(&p).unwrap_err();
    assert!(error.to_string().contains("declared state reset"));
    assert_eq!(
        std::fs::read(&old).unwrap(),
        b"old authority, never decoded"
    );
    assert_eq!(
        std::fs::read(&p.sqlite_path).unwrap(),
        b"unknown SQL version or corrupt database"
    );
    assert!(!p.state_dir.exists());
    assert!(!p.journal_dir.exists());
}
#[test]
fn manifest_rejects_relocated_root_and_symlink_without_creating_child() {
    let root = tempfile::tempdir().unwrap();
    let p = paths(root.path());
    let lease = StateLockGuard::acquire(&p.state_dir).unwrap();
    AuthorityManifest::admit(&p, &lease).unwrap();
    let mut changed = p.clone();
    changed.audit_dir = root.path().join("different");
    assert!(AuthorityManifest::preflight(&changed).is_err());
    assert!(!changed.audit_dir.exists());
    #[cfg(unix)]
    {
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&p.audit_dir, &alias).unwrap();
        changed.audit_dir = alias.join("child");
        assert!(AuthorityManifest::preflight(&changed).is_err());
        assert!(!p.audit_dir.join("child").exists());
    }
}
