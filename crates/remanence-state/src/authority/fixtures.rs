//! Semantic authority fixtures. Tests supply commits, rows and immutable plans;
//! only the selected backend knows their file names and host encodings.
use super::{TapeAuthority, TapeAuthorityWriter};
use crate::{StateError, TerminalFinalizationIntent};
use std::path::{Path, PathBuf};

/// Builder shared by behavior tests and offline fixture producers.
#[derive(Debug)]
pub struct AuthorityBuilder {
    directory: PathBuf,
    tape_uuid: [u8; 16],
    commits: Vec<super::AuthorityCommit>,
    plan: Option<TerminalFinalizationIntent>,
    rows: Vec<remanence_parity::authority::ShadowPublication>,
    geometry: Option<(u32, remanence_parity::ParityScheme)>,
}
impl AuthorityBuilder {
    /// Select fixture storage without naming a journal file or encoding.
    pub fn new(directory: impl AsRef<Path>, tape_uuid: [u8; 16]) -> Self {
        Self {
            directory: directory.as_ref().to_path_buf(),
            tape_uuid,
            commits: Vec::new(),
            plan: None,
            rows: Vec::new(),
            geometry: None,
        }
    }
    /// Supply geometry for a negative fixture with structural claims but no commit.
    pub fn geometry(mut self, block_size: u32, scheme: remanence_parity::ParityScheme) -> Self {
        self.geometry = Some((block_size, scheme));
        self
    }
    /// Add a complete semantic media commit in publication order.
    pub fn commit(mut self, commit: impl Into<super::AuthorityCommit>) -> Self {
        self.commits.push(commit.into());
        self
    }
    /// Add the structural rows published for these commits, without naming a host file.
    pub fn rows(mut self, report: remanence_parity::authority::ShadowPublication) -> Self {
        self.rows.push(report);
        self
    }
    /// Publish an immutable finalization plan after the supplied commits.
    pub fn plan(mut self, plan: TerminalFinalizationIntent) -> Self {
        self.plan = Some(plan);
        self
    }
    /// Build one exclusive writer retaining both semantic authority facets.
    pub fn build_lease(self) -> Result<super::TapeAuthorityLease, StateError> {
        let geometry = self.geometry.clone().or_else(|| {
            self.commits.first().and_then(|commit| {
                commit
                    .scheme
                    .clone()
                    .map(|scheme| (commit.block_size, scheme))
            })
        });
        let pending = self.plan.is_some();
        let authority = self.build()?;
        let mut writer = if pending {
            authority.acquire_exclusive_for_terminal_recovery()?
        } else {
            authority.acquire_exclusive()?
        };
        if let Some(geometry) = geometry {
            let rows = writer
                .open_structural(geometry.0, geometry.1)
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
            writer.retain_structural_publisher(rows)?;
        }
        Ok(writer)
    }
    /// Materialize the selected old implementation through the writer contract.
    pub fn build(self) -> Result<TapeAuthority, StateError> {
        std::fs::create_dir_all(&self.directory)
            .map_err(|e| StateError::io_at("create fixture tape directory", &self.directory, e))?;
        let authority = TapeAuthority::open_fixture(&self.directory, self.tape_uuid)?;
        if self.commits.is_empty() && !self.rows.is_empty() {
            std::fs::write(authority.path(), b"unclosed-structural-authority")
                .map_err(|e| StateError::io_at("write conflicting fixture", authority.path(), e))?;
            return Ok(authority);
        }
        if !self.commits.is_empty() || self.plan.is_some() {
            let mut lease = authority.acquire_exclusive()?;
            for commit in self.commits {
                lease.publish_report(&commit)?;
            }
            if let Some(plan) = self.plan {
                lease.begin_finalization(&(&plan).into())?;
            }
        }
        Ok(authority)
    }
}

/// Opaque evidence of deliberately damaged fixture authority. Tests can check
/// preservation without learning the backend's framing or filenames.
pub struct DamagedAuthorityEvidence {
    authority: TapeAuthority,
    bytes: Vec<u8>,
}
impl DamagedAuthorityEvidence {
    /// Whether a refusing operation left the exact original evidence untouched.
    pub fn is_unchanged(&self) -> Result<bool, StateError> {
        std::fs::read(self.authority.path())
            .map(|bytes| bytes == self.bytes)
            .map_err(|e| StateError::io_at("read fixture", self.authority.path(), e))
    }
}

/// Replace the selected fixture authority with unreadable evidence. This
/// expresses refusal behavior; the backend chooses the invalid host encoding.
pub fn damage_authority(authority: TapeAuthority) -> Result<DamagedAuthorityEvidence, StateError> {
    let bytes = b"corrupt-tape-log".to_vec();
    std::fs::write(authority.path(), &bytes)
        .map_err(|e| StateError::io_at("damage fixture", authority.path(), e))?;
    Ok(DamagedAuthorityEvidence { authority, bytes })
}

/// Shared capability used to exercise contention without exposing lock files.
pub struct SharedFixtureLease {
    _lock: nix::fcntl::Flock<remanence_library::durable_file::File>,
}

/// Hold a shared authority lease for a test's contention interval.
pub fn hold_shared(authority: &TapeAuthority) -> Result<SharedFixtureLease, StateError> {
    use nix::fcntl::{Flock, FlockArg};
    let path = authority.path().with_extension("lock");
    let file = remanence_library::durable_file::File::options()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .map_err(|e| StateError::io_at("fixture lock", &path, e))?;
    let lock = Flock::lock(file, FlockArg::LockSharedNonblock)
        .map_err(|(_, e)| StateError::JournalReplayFailed(e.to_string()))?;
    Ok(SharedFixtureLease { _lock: lock })
}

/// Observe a publication from a synchronous test transport callback. This probe
/// does not acquire a second lock while the test actor owns its writer lease.
pub fn has_publication(authority: &TapeAuthority) -> bool {
    authority.path().exists()
}

/// Locate a structural fixture without duplicating the selected backend's naming rule.
pub fn structural_location(
    checkpoint_directory: &Path,
    tape_uuid: [u8; 16],
) -> Result<PathBuf, StateError> {
    Ok(checkpoint_directory.join(format!(
        "{}.remtape",
        uuid::Uuid::from_bytes(tape_uuid).simple()
    )))
}

/// Exact saved intent evidence for a stale-companion crash fixture.
pub struct SavedFinalization {
    path: PathBuf,
    bytes: Vec<u8>,
}
impl SavedFinalization {
    /// Restore the exact evidence after simulating a crash past commit fsync.
    pub fn restore(self) -> std::io::Result<()> {
        std::fs::write(self.path, self.bytes)
    }
}
/// Capture the selected backend's pending-intent evidence.
pub fn save_finalization(authority: &TapeAuthority) -> std::io::Result<SavedFinalization> {
    let path = authority.path().to_path_buf();
    let bytes = std::fs::read(&path)?;
    Ok(SavedFinalization { path, bytes })
}
/// A deliberately blocked intent-publication seam.
pub struct BlockedFinalization {
    _registration: remanence_library::durable_file::test_support::Registration,
}
impl BlockedFinalization {
    /// Remove the injected storage failure before retrying publication.
    pub fn unblock(self) -> std::io::Result<()> {
        drop(self);
        Ok(())
    }
}
/// Refuse staging-file creation before any finalization byte is appended.
pub fn block_finalization(authority: &TapeAuthority) -> std::io::Result<BlockedFinalization> {
    use remanence_library::durable_file::{DurableDirectory, DurableFile, StdDirectory};
    struct RefuseStaging;
    impl DurableDirectory for RefuseStaging {
        fn create(&self, path: &Path) -> std::io::Result<Box<dyn DurableFile>> {
            if path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".spool-"))
            {
                return Err(std::io::Error::other(
                    "injected finalization staging failure",
                ));
            }
            StdDirectory.create(path)
        }
        fn open(&self, path: &Path) -> std::io::Result<Box<dyn DurableFile>> {
            StdDirectory.open(path)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            StdDirectory.rename(from, to)
        }
        fn rename_noreplace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            StdDirectory.rename_noreplace(from, to)
        }
        fn sync(&self, path: &Path) -> std::io::Result<()> {
            StdDirectory.sync(path)
        }
        fn set_len(&self, path: &Path, len: u64) -> std::io::Result<()> {
            StdDirectory.set_len(path, len)
        }
    }
    Ok(BlockedFinalization {
        _registration: remanence_library::durable_file::test_support::register(
            authority.path().parent().unwrap().to_path_buf(),
            std::sync::Arc::new(RefuseStaging),
        ),
    })
}
/// Opaque matcher for storage fault injection at this authority's commit seam.
pub struct PublicationTarget {
    path: PathBuf,
}
impl PublicationTarget {
    /// Match a file-model event without exposing the backend's filename to the test.
    pub fn matches(&self, event_path: &Path) -> bool {
        self.path == event_path
            || (event_path.parent() == self.path.parent()
                && event_path.file_name().is_some_and(|name| {
                    name.to_string_lossy().starts_with(&format!(
                        ".initial-{}-",
                        self.path.file_stem().unwrap().to_string_lossy()
                    ))
                }))
    }
}
/// Bind a storage fault injector to this authority's commit member.
pub fn publication_target(authority: &TapeAuthority) -> PublicationTarget {
    PublicationTarget {
        path: authority.path().to_path_buf(),
    }
}
