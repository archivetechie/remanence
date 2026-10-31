//! File durability boundary shared by the state and parity journals.
//!
//! Append and sync remain separate operations. The host file retains replay,
//! seeking, metadata, and locking; a test backend substitutes durability and
//! mirrors written bytes into that host file for existing replay code.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Append and durability operations required by journal authorities.
pub trait DurableFile: Send {
    /// Append bytes without promising that they survive power loss.
    fn append(&mut self, bytes: &[u8]) -> io::Result<()>;
    /// Prove all prior bytes durable, or return an error.
    fn sync(&mut self) -> io::Result<()>;
    /// Prove file data (and metadata needed to retrieve it) durable. Test
    /// backends may conservatively use their existing full-sync model.
    fn sync_data(&mut self) -> io::Result<()> {
        self.sync()
    }
}

impl DurableFile for fs::File {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_all(bytes)
    }
    fn sync(&mut self) -> io::Result<()> {
        self.sync_all()
    }
    fn sync_data(&mut self) -> io::Result<()> {
        fs::File::sync_data(self)
    }
}

/// Namespace operations accompanying append-only files.
/// Paths identify files independently of handles, including across rename.
pub trait DurableDirectory: Send + Sync {
    /// Create an empty file, replacing its contents if it exists.
    fn create(&self, path: &Path) -> io::Result<Box<dyn DurableFile>>;
    /// Open an existing file for append.
    fn open(&self, path: &Path) -> io::Result<Box<dyn DurableFile>>;
    /// Rename a directory entry; durability still requires directory sync.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Publish a new name without ever replacing an existing entry.
    fn rename_noreplace(&self, _from: &Path, _to: &Path) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "backend lacks no-replace publication",
        ))
    }
    /// Make the directory's current entries durable.
    fn sync(&self, path: &Path) -> io::Result<()>;
    /// Truncate a file during rollback or explicit tail recovery.
    fn set_len(&self, path: &Path, len: u64) -> io::Result<()>;
}

/// Ordinary filesystem implementation, with no implicit syncs.
#[derive(Debug, Default)]
pub struct StdDirectory;

impl DurableDirectory for StdDirectory {
    fn create(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        Ok(Box::new(fs::File::create(path)?))
    }
    fn open(&self, path: &Path) -> io::Result<Box<dyn DurableFile>> {
        Ok(Box::new(fs::OpenOptions::new().append(true).open(path)?))
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }
    fn rename_noreplace(&self, from: &Path, to: &Path) -> io::Result<()> {
        host_rename_noreplace(from, to)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        fs::File::open(path)?.sync_all()
    }
    fn set_len(&self, path: &Path, len: u64) -> io::Result<()> {
        fs::OpenOptions::new().write(true).open(path)?.set_len(len)
    }
}

/// A host file whose append and sync operations pass through [`DurableFile`].
/// Reads, locks, and metadata keep their normal host semantics.
pub struct File {
    inner: fs::File,
    context: std::sync::Arc<FileContext>,
}

struct FileContext {
    path: PathBuf,
    #[cfg(feature = "test-support")]
    backend: Option<std::sync::Arc<dyn DurableDirectory>>,
    #[cfg(feature = "test-support")]
    writer: Option<std::sync::Mutex<Box<dyn DurableFile>>>,
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File")
            .field("path", &self.context.path)
            .finish()
    }
}

impl File {
    fn wrap(inner: fs::File, path: &Path, created: bool) -> io::Result<Self> {
        #[cfg(feature = "test-support")]
        let backend = test_support::backend(path);
        #[cfg(feature = "test-support")]
        let writer = if inner.metadata()?.is_file() {
            backend
                .as_ref()
                .map(|backend| {
                    if created {
                        backend.create(path)
                    } else {
                        backend.open(path)
                    }
                })
                .transpose()?
                .map(std::sync::Mutex::new)
        } else {
            None
        };
        #[cfg(not(feature = "test-support"))]
        let _ = created;
        Ok(Self {
            inner,
            context: std::sync::Arc::new(FileContext {
                path: path.to_owned(),
                #[cfg(feature = "test-support")]
                backend,
                #[cfg(feature = "test-support")]
                writer,
            }),
        })
    }

    /// Open a file for reading, or a directory for sync.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        Self::wrap(fs::File::open(path)?, path, false)
    }

    /// Create or truncate a writable file.
    pub fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
    }

    /// Return an options builder with standard filesystem semantics.
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    /// Sync bytes or directory entries without changing the existing sync order.
    pub fn sync_all(&self) -> io::Result<()> {
        #[cfg(feature = "test-support")]
        if let Some(backend) = &self.context.backend {
            if let Some(writer) = &self.context.writer {
                writer.lock().expect("durable writer lock").sync()?;
            } else {
                backend.sync(&self.context.path)?;
            }
        }
        self.inner.sync_all()
    }

    /// Sync provisional file data before the media barrier, without a namespace
    /// sync or any claim that the containing commit group is authoritative.
    pub fn sync_data(&self) -> io::Result<()> {
        #[cfg(feature = "test-support")]
        if let Some(writer) = &self.context.writer {
            writer.lock().expect("durable writer lock").sync_data()?;
        }
        self.inner.sync_data()
    }

    /// Change length without making the change durable.
    pub fn set_len(&self, len: u64) -> io::Result<()> {
        #[cfg(feature = "test-support")]
        if let Some(backend) = &self.context.backend {
            backend.set_len(&self.context.path, len)?;
        }
        self.inner.set_len(len)
    }

    /// Read ordinary host metadata.
    pub fn metadata(&self) -> io::Result<fs::Metadata> {
        self.inner.metadata()
    }

    /// Duplicate the host handle, preserving its shared cursor and backend.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            inner: self.inner.try_clone()?,
            context: self.context.clone(),
        })
    }
}

impl DurableFile for File {
    fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        #[cfg(feature = "test-support")]
        if let Some(writer) = &self.context.writer {
            writer.lock().expect("durable writer lock").append(bytes)?;
        }
        DurableFile::append(&mut self.inner, bytes)
    }
    fn sync(&mut self) -> io::Result<()> {
        self.sync_all()
    }
    fn sync_data(&mut self) -> io::Result<()> {
        File::sync_data(self)
    }
}

impl Write for File {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.append(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl Read for File {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.inner.read(bytes)
    }
}
impl Seek for File {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}
#[cfg(unix)]
impl std::os::fd::AsRawFd for File {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.inner.as_raw_fd()
    }
}
#[cfg(unix)]
// SAFETY: File owns the descriptor for the full lifetime of the lock guard.
unsafe impl nix::fcntl::Flockable for File {}

/// Standard open options with durability backend attachment at open time.
#[derive(Debug)]
pub struct OpenOptions {
    inner: fs::OpenOptions,
    truncate: bool,
}
impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}
impl OpenOptions {
    /// Start with no access or creation flags.
    pub fn new() -> Self {
        Self {
            inner: fs::OpenOptions::new(),
            truncate: false,
        }
    }
    /// Enable read access.
    pub fn read(&mut self, value: bool) -> &mut Self {
        self.inner.read(value);
        self
    }
    /// Enable write access.
    pub fn write(&mut self, value: bool) -> &mut Self {
        self.inner.write(value);
        self
    }
    /// Append to the current end of file.
    pub fn append(&mut self, value: bool) -> &mut Self {
        self.inner.append(value);
        self
    }
    /// Create a missing file.
    pub fn create(&mut self, value: bool) -> &mut Self {
        self.inner.create(value);
        self
    }
    /// Require a newly created file.
    pub fn create_new(&mut self, value: bool) -> &mut Self {
        self.inner.create_new(value);
        self
    }
    /// Truncate an existing file on open.
    pub fn truncate(&mut self, value: bool) -> &mut Self {
        self.inner.truncate(value);
        self.truncate = value;
        self
    }
    /// Open with the selected flags.
    pub fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let path = path.as_ref();
        let created = self.truncate || !path.try_exists()?;
        File::wrap(self.inner.open(path)?, path, created)
    }
}

/// Rename a file without adding a directory sync.
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    #[cfg(feature = "test-support")]
    if let Some(backend) = test_support::backend(from) {
        backend.rename(from, to)?;
    }
    fs::rename(from, to)
}

/// Scoped backend substitution used only by hermetic crash tests.
#[cfg(feature = "test-support")]
pub mod test_support;

/// Atomically publish a newly created file, refusing any existing target. The
/// caller must sync the containing directory before reporting durability.
pub fn rename_noreplace(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    #[cfg(feature = "test-support")]
    if let Some(backend) = test_support::backend(from) {
        backend.rename_noreplace(from, to)?;
    }
    host_rename_noreplace(from, to)
}
fn host_rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    // link is atomic and fails with AlreadyExists. Both names are in the same
    // directory/filesystem. An interrupted unlink leaves only an unused temp name.
    fs::hard_link(from, to)?;
    fs::remove_file(from)
}
