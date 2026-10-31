//! In-memory append durability with byte-level tears and independent directory entries.
//!
//! Sync copies written bytes to durable bytes. A failed sync discards the dirty
//! bytes and consumes the failure, so retrying sync cannot resurrect lost data.
//! Snapshots let tests explore each tail and namespace choice independently.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use remanence_library::durable_file::{DurableDirectory, DurableFile};

/// Bytes of an inode, independent of the names that refer to it.
#[derive(Clone, Debug, Default)]
pub struct FileBytes {
    /// Bytes currently visible to reads.
    pub written: Vec<u8>,
    /// Bytes proved durable by the last successful sync.
    pub durable: Vec<u8>,
    fail_sync: bool,
}

/// A complete filesystem durability snapshot. Cloning it forks a crash experiment.
#[derive(Clone, Debug, Default)]
pub struct FileSnapshot {
    files: Vec<FileBytes>,
    known_paths: BTreeSet<PathBuf>,
    entries: BTreeMap<PathBuf, usize>,
    durable_entries: BTreeMap<PathBuf, usize>,
}

impl FileSnapshot {
    /// Visible paths and the bytes attached to each path.
    pub fn files(&self) -> impl Iterator<Item = (&Path, &FileBytes)> {
        self.entries
            .iter()
            .map(|(path, id)| (path.as_path(), &self.files[*id]))
    }

    /// Crash, choosing the number of unsynced tail bytes retained per file.
    /// Missing choices retain zero bytes. `keep_entries` chooses either all
    /// current names or the last directory-synced names. Neither choice syncs data.
    /// The append model rejects non-prefix dirty data rather than inventing a tear.
    pub fn crash(
        &mut self,
        tails: &BTreeMap<PathBuf, usize>,
        keep_entries: bool,
    ) -> io::Result<()> {
        let mut choices = BTreeMap::new();
        for (path, keep) in tails {
            let id = self
                .entries
                .get(path)
                .or_else(|| self.durable_entries.get(path))
                .ok_or_else(missing_file)?;
            if let Some(previous) = choices.insert(*id, *keep) {
                if previous != *keep {
                    return Err(io::Error::other("conflicting tears for renamed inode"));
                }
            }
        }
        let mut retained = BTreeMap::new();
        for id in self.entries.values().chain(self.durable_entries.values()) {
            let bytes = &self.files[*id];
            let keep = choices.get(id).copied().unwrap_or(0);
            if !bytes.written.starts_with(&bytes.durable) {
                if keep != 0 {
                    return Err(io::Error::other("torn overwrite is not an append tail"));
                }
                retained.entry(*id).or_insert_with(|| bytes.durable.clone());
                continue;
            }
            let end = bytes
                .durable
                .len()
                .checked_add(keep)
                .filter(|end| *end <= bytes.written.len())
                .ok_or_else(|| io::Error::other("retained tail exceeds written bytes"))?;
            let candidate = bytes.written[..end].to_vec();
            if let Some(previous) = retained.insert(*id, candidate.clone()) {
                if previous != candidate {
                    return Err(io::Error::other("conflicting tears for renamed inode"));
                }
            }
        }
        for (id, bytes) in retained {
            self.files[id].written = bytes.clone();
            self.files[id].durable = bytes;
            self.files[id].fail_sync = false;
        }
        if !keep_entries {
            self.entries = self.durable_entries.clone();
        }
        self.durable_entries = self.entries.clone();
        Ok(())
    }

    /// Materialize surviving names and bytes into an isolated state directory.
    /// Remove tracked names that the selected namespace outcome loses.
    pub fn materialize(&self, root: &Path, destination: &Path) -> io::Result<()> {
        for path in &self.known_paths {
            let relative = path.strip_prefix(root).map_err(io::Error::other)?;
            let target = destination.join(relative);
            if !self.entries.contains_key(path) && target.exists() {
                std::fs::remove_file(target)?;
            }
        }
        for (path, bytes) in self.files() {
            let target = destination.join(path.strip_prefix(root).map_err(io::Error::other)?);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(target, &bytes.written)?;
        }
        Ok(())
    }
}

/// A durability operation observable at a crash boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileEvent {
    /// Bytes become visible but remain unsynced.
    Append(PathBuf),
    /// Sync is about to run; a crash here can tear the preceding append.
    BeforeSync(PathBuf),
    /// Sync completes, successfully or with data loss.
    Sync(PathBuf, bool),
    /// A file is created.
    Create(PathBuf),
    /// An entry is renamed.
    Rename(PathBuf, PathBuf),
    /// A directory's current entries become durable.
    DirectorySync(PathBuf),
    /// A rollback changes visible length.
    Truncate(PathBuf),
}

type Observer = Arc<dyn Fn(FileEvent) + Send + Sync>;

/// Shared in-memory filesystem, usable directly or through the host-file seam.
#[derive(Clone, Default)]
pub struct MemoryDirectory {
    state: Arc<Mutex<FileSnapshot>>,
    observer: Arc<Mutex<Option<Observer>>>,
}

impl MemoryDirectory {
    /// Fork a saved state without retaining observers or host-file handles.
    pub fn from_snapshot(snapshot: FileSnapshot) -> Self {
        Self {
            state: Arc::new(Mutex::new(snapshot)),
            observer: Arc::default(),
        }
    }

    /// Capture all written and durable bytes and namespace state.
    pub fn snapshot(&self) -> FileSnapshot {
        self.state.lock().expect("file model lock").clone()
    }

    /// Observe boundaries without holding the filesystem lock.
    pub fn observe(&self, observer: Option<Observer>) {
        *self.observer.lock().expect("file observer lock") = observer;
    }

    fn event(&self, event: FileEvent) {
        let observer = self.observer.lock().expect("file observer lock").clone();
        if let Some(observer) = observer {
            observer(event);
        }
    }

    /// Seed an existing durable file before attaching a model to an engine.
    pub fn seed(&self, path: PathBuf, bytes: Vec<u8>) {
        let mut state = self.state.lock().expect("file model lock");
        let id = state.files.len();
        state.files.push(FileBytes {
            written: bytes.clone(),
            durable: bytes,
            fail_sync: false,
        });
        state.known_paths.insert(path.clone());
        state.entries.insert(path.clone(), id);
        state.durable_entries.insert(path, id);
    }

    /// Fail the next sync of this inode, including through an already open handle.
    pub fn fail_next_sync(&self, path: &Path) -> io::Result<()> {
        let mut state = self.state.lock().expect("file model lock");
        let id = *state.entries.get(path).ok_or_else(missing_file)?;
        state.files[id].fail_sync = true;
        Ok(())
    }
}

fn missing_file() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "missing model file")
}

struct MemoryFile {
    directory: MemoryDirectory,
    id: usize,
    path: PathBuf,
}
impl DurableFile for MemoryFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.directory.state.lock().expect("file model lock").files[self.id]
            .written
            .extend_from_slice(bytes);
        self.directory.event(FileEvent::Append(self.path.clone()));
        Ok(())
    }
    fn sync(&mut self) -> io::Result<()> {
        self.directory
            .event(FileEvent::BeforeSync(self.path.clone()));
        let failed = {
            let mut state = self.directory.state.lock().expect("file model lock");
            let file = &mut state.files[self.id];
            let failed = std::mem::take(&mut file.fail_sync);
            if failed {
                file.written = file.durable.clone();
            } else {
                file.durable = file.written.clone();
            }
            failed
        };
        self.directory
            .event(FileEvent::Sync(self.path.clone(), !failed));
        if failed {
            Err(io::Error::other(
                "injected fsync failure; dirty bytes are lost",
            ))
        } else {
            Ok(())
        }
    }
}

impl DurableDirectory for MemoryDirectory {
    fn create(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        let id = {
            let mut state = self.state.lock().expect("file model lock");
            state.known_paths.insert(path.to_owned());
            // O_TRUNC preserves inode identity when the file already exists.
            if let Some(id) = state.entries.get(path).copied() {
                state.files[id].written.clear();
                id
            } else {
                let id = state.files.len();
                state.files.push(FileBytes::default());
                state.entries.insert(path.to_owned(), id);
                id
            }
        };
        self.event(FileEvent::Create(path.to_owned()));
        Ok(Box::new(MemoryFile {
            directory: self.clone(),
            id,
            path: path.to_owned(),
        }))
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        let id = *self
            .state
            .lock()
            .expect("file model lock")
            .entries
            .get(path)
            .ok_or_else(missing_file)?;
        Ok(Box::new(MemoryFile {
            directory: self.clone(),
            id,
            path: path.to_owned(),
        }))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        {
            let mut state = self.state.lock().expect("file model lock");
            let id = state.entries.remove(from).ok_or_else(missing_file)?;
            state.known_paths.insert(to.to_owned());
            state.entries.insert(to.to_owned(), id);
        }
        self.event(FileEvent::Rename(from.to_owned(), to.to_owned()));
        Ok(())
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        {
            let mut state = self.state.lock().expect("file model lock");
            if state.entries.contains_key(to) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "publication target exists",
                ));
            }
            let id = state.entries.remove(from).ok_or_else(missing_file)?;
            state.known_paths.insert(to.to_owned());
            state.entries.insert(to.to_owned(), id);
        }
        self.event(FileEvent::Rename(from.to_owned(), to.to_owned()));
        Ok(())
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        {
            let mut state = self.state.lock().expect("file model lock");
            state
                .durable_entries
                .retain(|entry, _| entry.parent() != Some(path));
            let entries: Vec<_> = state
                .entries
                .iter()
                .filter(|(entry, _)| entry.parent() == Some(path))
                .map(|(entry, id)| (entry.clone(), *id))
                .collect();
            state.durable_entries.extend(entries);
        }
        self.event(FileEvent::DirectorySync(path.to_owned()));
        Ok(())
    }
    fn set_len(&self, path: &Path, len: u64) -> io::Result<()> {
        let len = usize::try_from(len).map_err(io::Error::other)?;
        {
            let mut state = self.state.lock().expect("file model lock");
            let id = *state.entries.get(path).ok_or_else(missing_file)?;
            state.files[id].written.resize(len, 0);
        }
        self.event(FileEvent::Truncate(path.to_owned()));
        Ok(())
    }
}
