//! Path-scoped injection allows concurrent engines to use isolated durability models.

use super::DurableDirectory;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

type Entry = (PathBuf, Arc<dyn DurableDirectory>);
fn registry() -> &'static Mutex<Vec<Entry>> {
    static REGISTRY: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

/// Retains a backend registration until all work in its subtree finishes.
pub struct Registration(PathBuf);

/// Substitute durability for files beneath an absolute, non-overlapping root.
/// Drop all open handles before dropping this registration.
pub fn register(root: PathBuf, backend: Arc<dyn DurableDirectory>) -> Registration {
    assert!(root.is_absolute(), "durability root must be absolute");
    let mut entries = registry().lock().expect("durability registry lock");
    assert!(
        entries
            .iter()
            .all(|(other, _)| !other.starts_with(&root) && !root.starts_with(other)),
        "durability roots overlap"
    );
    entries.push((root.clone(), backend));
    Registration(root)
}

pub(super) fn backend(path: &Path) -> Option<Arc<dyn DurableDirectory>> {
    registry()
        .lock()
        .expect("durability registry lock")
        .iter()
        .find(|(root, _)| path.starts_with(root))
        .map(|(_, backend)| backend.clone())
}

impl Drop for Registration {
    fn drop(&mut self) {
        registry()
            .lock()
            .expect("durability registry lock")
            .retain(|(root, _)| *root != self.0);
    }
}
