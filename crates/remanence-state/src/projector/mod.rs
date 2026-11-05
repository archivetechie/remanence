//! Process-local ownership of writable catalog connections.
//!
//! Clients issue non-cloneable writer handles. A handle locks only inside a
//! catalog method; neither the core nor its guard can escape the module.
//! Read-only catalogs have independent connections. SQL and transactions remain
//! in `index` and its tape-log projection child until the authority cutover.
#![doc = include_str!("catalog_writer.md")]

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

use crate::StateError;
use index::{CatalogCore, MediaReadinessObserver};

pub use index::apply;
mod handle;
pub mod index;
pub mod thread;

#[derive(Debug)]
struct SharedCore {
    core: Mutex<Option<CatalogCore>>,
    closed: Arc<Closed>,
}

// A weak reference expires before Arc runs the core destructor. Wait for SQLite
// close to finish before replacing that entry, including concurrent last drops.
#[derive(Debug, Default)]
struct Closed {
    done: Mutex<bool>,
    changed: Condvar,
}
impl Closed {
    fn wait(&self) {
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            done = self.changed.wait(done).unwrap_or_else(|e| e.into_inner());
        }
    }
}
impl SharedCore {
    fn new(core: CatalogCore) -> Arc<Self> {
        Arc::new(Self {
            core: Mutex::new(Some(core)),
            closed: Arc::default(),
        })
    }
}
impl Drop for SharedCore {
    fn drop(&mut self) {
        drop(
            self.core
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
        *self.closed.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.closed.changed.notify_all();
    }
}
struct RegistryEntry {
    core: Weak<SharedCore>,
    closed: Arc<Closed>,
}
impl RegistryEntry {
    fn upgrade_or_wait(&self) -> Option<Arc<SharedCore>> {
        let core = self.core.upgrade();
        if core.is_none() {
            self.closed.wait();
        }
        core
    }
}
type Registry = HashMap<PathBuf, RegistryEntry>;
static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
thread_local! {
    static HELD: RefCell<HashSet<usize>> = RefCell::new(HashSet::new());
}

// Insertion precedes locking; dropping removes the flag on both return and unwind.
struct Entered(usize);
impl Entered {
    fn enter(core: &Arc<SharedCore>) -> Result<Self, StateError> {
        let key = Arc::as_ptr(core) as usize;
        if HELD.with(|held| held.borrow_mut().insert(key)) {
            Ok(Self(key))
        } else {
            Err(StateError::CatalogReentered)
        }
    }
}
impl Drop for Entered {
    fn drop(&mut self) {
        HELD.with(|held| {
            held.borrow_mut().remove(&self.0);
        });
    }
}

/// Cloneable owner of the process's single writable connection to a catalog.
#[derive(Clone, Debug)]
pub struct ProjectionClient {
    core: Arc<SharedCore>,
    path: PathBuf,
}
impl ProjectionClient {
    /// Issue a fresh writer with no readiness observer.
    pub fn write_handle(&self) -> CatalogIndex {
        CatalogIndex {
            core: self.core.clone(),
            path: self.path.clone(),
            readiness_observer: None,
        }
    }
    /// Path supplied by this client's owner.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Writer capability issued only by a projector client; no constructor or clone.
#[derive(Debug)]
pub struct CatalogIndex {
    core: Arc<SharedCore>,
    path: PathBuf,
    readiness_observer: Option<Arc<dyn MediaReadinessObserver>>,
}
impl CatalogIndex {
    /// Subscribe only this handle to durable readiness changes.
    pub fn set_readiness_observer(&mut self, observer: Arc<dyn MediaReadinessObserver>) {
        self.readiness_observer = Some(observer);
    }
    /// Original path backing this handle.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn with_core<T>(
        &self,
        run: impl FnOnce(&mut CatalogCore) -> Result<T, StateError>,
    ) -> Result<T, StateError> {
        let _entered = Entered::enter(&self.core)?;
        let mut core = self
            .core
            .core
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        run(core.as_mut().expect("live catalog core"))
    }
}

/// An owned independent read-only connection; deliberately has no `DerefMut`.
#[derive(Debug)]
pub struct CatalogReader(CatalogIndex);
impl Deref for CatalogReader {
    type Target = CatalogIndex;
    fn deref(&self) -> &CatalogIndex {
        &self.0
    }
}
impl CatalogReader {
    fn from_core(core: CatalogCore, path: PathBuf) -> Self {
        Self(CatalogIndex {
            core: SharedCore::new(core),
            path,
            readiness_observer: None,
        })
    }
    /// Open an existing catalog read-only, with the current schema validation.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref();
        Ok(Self::from_core(
            CatalogCore::open_read_only(path)?,
            path.to_path_buf(),
        ))
    }
    /// Inspect an existing catalog without creating writable sidecars.
    pub fn open_inventory(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let core = CatalogCore::open_inventory(&path)?;
        let path = core.path().to_path_buf();
        Ok(Self::from_core(core, path))
    }
    /// Build the current schema privately in memory and expose only reads.
    pub fn empty() -> Result<Self, StateError> {
        Ok(Self::from_core(
            CatalogCore::open(":memory:")?,
            PathBuf::from(":memory:"),
        ))
    }
}

fn registry_key(path: &Path) -> Result<PathBuf, StateError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| StateError::io_at("create sqlite directory", parent, e))?;
    let parent = std::fs::canonicalize(parent)
        .map_err(|e| StateError::io_at("resolve catalog directory", parent, e))?;
    Ok(parent.join(path.file_name().unwrap_or_default()))
}

/// Get or open the single writable connection for a canonical catalog path.
/// In-memory catalogs are private and never enter the process registry.
pub fn open(path: impl AsRef<Path>) -> Result<ProjectionClient, StateError> {
    let path = path.as_ref();
    if path == Path::new(":memory:") {
        return Ok(ProjectionClient {
            core: SharedCore::new(CatalogCore::open(path)?),
            path: path.to_path_buf(),
        });
    }
    let key = registry_key(path)?;
    let mut registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    registry.retain(|_, value| !*value.closed.done.lock().unwrap_or_else(|e| e.into_inner()));
    let core = match registry.get(&key).and_then(RegistryEntry::upgrade_or_wait) {
        Some(core) => core,
        None => {
            let core = SharedCore::new(CatalogCore::open(path)?);
            registry.insert(
                key,
                RegistryEntry {
                    core: Arc::downgrade(&core),
                    closed: core.closed.clone(),
                },
            );
            core
        }
    };
    Ok(ProjectionClient {
        core,
        path: path.to_path_buf(),
    })
}

/// Reset-only compatible predecessor reader; ordinary readers keep their gates.
pub(crate) fn open_reset_source(path: &Path) -> Result<CatalogReader, StateError> {
    Ok(CatalogReader::from_core(
        CatalogCore::open_reset_source(path)?,
        path.to_path_buf(),
    ))
}

/// The replacement has its own registry key until the existing atomic swap.
pub(crate) fn open_reset_replacement(path: &Path) -> Result<CatalogIndex, StateError> {
    Ok(open(path)?.write_handle())
}

/// Serialize the reset's special checkpoint opener with ordinary catalog opens.
pub(crate) fn prepare_admitted_catalog_reset_source_atomic_swap(
    path: impl AsRef<Path>,
    version: u32,
) -> Result<(), StateError> {
    let path = path.as_ref();
    let key = registry_key(path)?;
    let registry = REGISTRY
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if registry
        .get(&key)
        .and_then(RegistryEntry::upgrade_or_wait)
        .is_some()
    {
        return Err(StateError::ConfigInvalid(
            "catalog reset refused while a shared catalog connection is live".into(),
        ));
    }
    CatalogCore::prepare_admitted_catalog_reset_source_atomic_swap(path, version)
}

/// Exact-erase legacy reader remains confined to the reset path.
pub(crate) fn read_legacy_catalog_reset_erase_source(
    path: impl AsRef<Path>,
) -> Result<(u32, Vec<index::TapeRecord>), StateError> {
    CatalogCore::read_legacy_catalog_reset_erase_source(path)
}

#[cfg(test)]
mod b4a_tests;

#[cfg(test)]
pub(crate) fn memory_projection_fixture() -> CatalogIndex {
    CatalogIndex {
        core: SharedCore::new(CatalogCore::memory_projection_fixture()),
        path: PathBuf::from(":memory:"),
        readiness_observer: None,
    }
}

/// Test fixture for the future schema with no copy-level pool storage.
#[cfg(test)]
pub(crate) fn memory_projection_without_copy_pool_fixture() -> CatalogIndex {
    let writer = memory_projection_fixture();
    writer
        .with_core(|core| {
            core.remove_copy_pool_fixture();
            Ok(())
        })
        .unwrap();
    writer
}
