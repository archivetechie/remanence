//! Durable host publication shared by log families. Unknown outcomes retain a
//! sticky companion marker and an in-process fence, even when marker I/O fails.
//! Replacement copies an immutable prefix to a new inode; it never truncates.

use remanence_library::durable_file::{self, File};
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Unique unpublished file. The caller supplies the family identity used in its
/// cleanup namespace; no log-family header or record is interpreted here.
pub struct Temporary {
    /// Unique path in the target directory.
    pub path: PathBuf,
    /// Open read/write file routed through the durability boundary.
    pub file: File,
    /// Retain evidence instead of deleting the temporary on drop.
    pub keep: bool,
}
impl Temporary {
    /// Create without replacing any existing temporary.
    pub fn new(directory: &Path, identity: &str, purpose: &str) -> io::Result<Self> {
        let path = directory.join(format!(".{purpose}-{identity}-{}", uuid::Uuid::new_v4()));
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            path,
            file,
            keep: false,
        })
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn process_fences() -> &'static Mutex<BTreeSet<PathBuf>> {
    static FENCES: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
    FENCES.get_or_init(Mutex::default)
}
/// Test both the durable marker and the sticky fallback for failed marker I/O.
pub fn has_ambiguity(path: &Path) -> io::Result<bool> {
    Ok(process_fences()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(path)
        || path.with_extension("ambiguous").try_exists()?)
}
/// Remember first. Attempt directory sync even when the marker's own sync fails.
pub fn fence_ambiguity(directory: &Path, path: &Path) {
    process_fences()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(path.to_path_buf());
    let file_result = File::options()
        .write(true)
        .create(true)
        .open(path.with_extension("ambiguous"))
        .and_then(|file| file.sync_all());
    let directory_result = File::open(directory).and_then(|dir| dir.sync_all());
    if let Err(error) = file_result.and(directory_result) {
        tracing::error!(path = %path.display(), %error, "could not durably persist ambiguity fence; process fence remains");
    }
}
/// Clear only the process fallback after the family has completed explicit
/// recovery and durably removed the marker. Does not authorize recovery itself.
pub fn clear_process_fence(path: &Path) {
    process_fences()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(path);
}

/// Sync a completed temporary, publish without replacement, then sync its name.
/// The caller must fence an uncertain result; [`append_and_publish`] does so.
pub fn publish_new(
    file: &File,
    temporary: &Path,
    target: &Path,
    directory: &Path,
) -> io::Result<()> {
    file.sync_all()?;
    durable_file::rename_noreplace(temporary, target)?;
    File::open(directory)?.sync_all()
}

/// Append a prevalidated frame and establish durability. For first publication,
/// the already-written header/provisional prefix is in `temporary`. The optional
/// callback preserves a family's before-sync observer at the actual boundary.
/// Any write, observer, sync or namespace error sets the sticky marker once.
pub fn append_and_publish(
    output: &mut File,
    bytes: &[u8],
    temporary: Option<&Path>,
    target: &Path,
    directory: &Path,
    before_sync: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let result = (|| {
        output.write_all(bytes)?;
        before_sync()?;
        if let Some(temporary) = temporary {
            publish_new(output, temporary, target, directory)
        } else {
            output.sync_all()
        }
    })();
    if result.is_err() {
        fence_ambiguity(directory, target);
    }
    result
}

/// Distinguishes an invalid retained boundary from environmental publication I/O.
#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    /// The immutable source ended before the requested boundary.
    #[error("short repair source")]
    ShortSource,
    /// Caller retains its repair/ambiguity fence until recovery proves durability.
    #[error(transparent)]
    Io(#[from] io::Error),
}
/// Copy a retained immutable prefix and atomically replace the active inode.
/// Recovery policy, evidence archival and fence retirement belong to the family.
pub fn replace_prefix(
    directory: &Path,
    target: &Path,
    identity: &str,
    end: u64,
) -> Result<(), ReplaceError> {
    let mut temp = Temporary::new(directory, identity, "replacement")?;
    let source = fs::File::open(target)?;
    if io::copy(&mut source.take(end), &mut temp.file)? != end {
        return Err(ReplaceError::ShortSource);
    }
    temp.file.sync_all()?;
    durable_file::rename(&temp.path, target)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}
