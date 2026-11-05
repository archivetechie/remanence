//! Writer ownership, canonical registry identity and separate readers.
use remanence_state::{projector, CatalogReader};
use std::path::Path;
/// Count writable main-file descriptors, excluding WAL and SHM.
fn b4a_writable_connections(path: &Path) -> usize {
    let path = path.canonicalize().unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            if std::fs::read_link(entry.path()).ok().as_ref() != Some(&path) {
                return false;
            }
            let info =
                std::fs::read_to_string(Path::new("/proc/self/fdinfo").join(entry.file_name()))
                    .unwrap();
            let flags = info
                .lines()
                .find_map(|line| line.strip_prefix("flags:\t"))
                .unwrap();
            u32::from_str_radix(flags, 8).unwrap() & 3 == 2
        })
        .count()
}

/// Canonical and symlinked opens share one connection, released with its last handle.
#[test]
fn b4a_projector_open_shares_one_connection() {
    let dir = tempfile::tempdir_in(".").unwrap();
    let file = dir.path().join("catalog.sqlite");
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(dir.path().canonicalize().unwrap(), &alias).unwrap();
    let a = projector::open(&file).unwrap();
    let b = projector::open(alias.join("catalog.sqlite")).unwrap();
    let handle = b.write_handle();
    assert_eq!(b4a_writable_connections(&file), 1);
    let other_path = dir.path().join("other.sqlite");
    let other = projector::open(&other_path).unwrap();
    assert_eq!(b4a_writable_connections(&other_path), 1);
    let reader = CatalogReader::open(&file).unwrap();
    assert_eq!(
        reader.schema_version().unwrap(),
        remanence_state::projector::index::SCHEMA_VERSION
    );
    drop((a, b));
    assert_eq!(b4a_writable_connections(&file), 1);
    drop(handle);
    // Drop the reader too: SQLite can defer closing unused fds while any connection lives.
    drop(reader);
    assert_eq!(b4a_writable_connections(&file), 0);
    let reopened = projector::open(&file).unwrap();
    assert_eq!(
        reopened.write_handle().schema_version().unwrap(),
        remanence_state::projector::index::SCHEMA_VERSION
    );
    assert_eq!(b4a_writable_connections(&file), 1);
    drop((reopened, other));
}
/// Empty dry-run readers have the current schema without creating a catalog file.
#[test]
fn b4a_empty_reader_has_current_schema() {
    let dir = tempfile::tempdir().unwrap();
    let reader = CatalogReader::empty().unwrap();
    assert_eq!(
        reader.schema_version().unwrap(),
        remanence_state::projector::index::SCHEMA_VERSION
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// Concurrent last-drop/reopen cycles never overlap writable SQLite lifetimes.
#[test]
fn b4a_concurrent_open_and_last_drop_never_duplicate_writer() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("catalog.sqlite");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                for _ in 0..20 {
                    barrier.wait();
                    let client = projector::open(&path).unwrap();
                    assert_eq!(
                        client.write_handle().schema_version().unwrap(),
                        remanence_state::projector::index::SCHEMA_VERSION
                    );
                    assert_eq!(b4a_writable_connections(&path), 1);
                    drop(client);
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(b4a_writable_connections(&path), 0);
}
