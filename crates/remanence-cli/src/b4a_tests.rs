//! Dry-run catalog capability regression on temporary filesystem state.
use super::*;

/// Tape-init's actual absent-catalog path creates neither its file nor its parent.
#[test]
fn b4a_dry_run_writes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().join("absent");
    let path = parent.join("catalog.sqlite");
    let (reader, kind) = open_tape_init_dry_run_catalog(&path).unwrap();
    assert_eq!(kind, DryRunCatalog::Absent);
    assert_eq!(
        reader.schema_version().unwrap(),
        remanence_state::projector::index::SCHEMA_VERSION
    );
    assert!(!path.exists());
    assert!(!parent.exists());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
