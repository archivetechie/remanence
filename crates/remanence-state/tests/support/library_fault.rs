//! Fault backend shared by library-store durability tests.
use remanence_library::durable_file::{DurableDirectory, DurableFile};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};
pub struct Injection {
    pub stage: AtomicU8,
    pub after: bool,
}
impl Injection {
    fn trip(&self, stage: u8, effect: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        if self
            .stage
            .compare_exchange(stage, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            if self.after {
                effect()?;
            }
            Err(io::Error::other("injected unknown publication outcome"))
        } else {
            Ok(())
        }
    }
}
struct FaultFile {
    path: PathBuf,
    injection: Arc<Injection>,
}
impl DurableFile for FaultFile {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.injection.trip(1, || {
            fs::OpenOptions::new()
                .append(true)
                .open(&self.path)?
                .write_all(bytes)
        })
    }
    fn sync(&mut self) -> io::Result<()> {
        self.injection
            .trip(2, || fs::File::open(&self.path)?.sync_all())
    }
}
pub struct FaultDirectory(pub Arc<Injection>);
impl DurableDirectory for FaultDirectory {
    fn create(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        self.open(path)
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        Ok(Box::new(FaultFile {
            path: path.into(),
            injection: self.0.clone(),
        }))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.0.trip(3, || fs::rename(from, to))
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename(from, to)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        self.0.trip(4, || fs::File::open(path)?.sync_all())
    }
    fn set_len(&self, _: &Path, _: u64) -> io::Result<()> {
        panic!("publication must never truncate")
    }
}

/// Page-cache model: writes reach the host, but a crash restores only synced bytes.
#[derive(Clone, Default)]
pub struct PageCache(pub Arc<std::sync::Mutex<std::collections::BTreeMap<PathBuf, Vec<u8>>>>);
impl PageCache {
    pub fn crash(&self) {
        for (path, bytes) in self.0.lock().unwrap().iter() {
            fs::write(path, bytes).unwrap();
        }
    }
}
struct CachedFile {
    path: PathBuf,
    cache: PageCache,
}
impl DurableFile for CachedFile {
    fn append(&mut self, _: &[u8]) -> io::Result<()> {
        Ok(()) // File mirrors the write to the host after this hook.
    }
    fn sync(&mut self) -> io::Result<()> {
        self.cache
            .0
            .lock()
            .unwrap()
            .insert(self.path.clone(), fs::read(&self.path)?);
        Ok(())
    }
}
impl DurableDirectory for PageCache {
    fn create(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        self.0.lock().unwrap().insert(path.into(), vec![]);
        self.open(path)
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        let mut cache = self.0.lock().unwrap();
        if !cache.contains_key(path) {
            cache.insert(path.into(), fs::read(path)?);
        }
        Ok(Box::new(CachedFile {
            path: path.into(),
            cache: self.clone(),
        }))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut cache = self.0.lock().unwrap();
        let bytes = cache.remove(from).expect("registered source");
        cache.insert(to.into(), bytes);
        Ok(())
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.rename(from, to)
    }
    fn sync(&self, _: &Path) -> io::Result<()> {
        Ok(())
    }
    fn set_len(&self, _: &Path, _: u64) -> io::Result<()> {
        panic!("no rollback")
    }
}
