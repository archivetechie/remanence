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
        let directory = self.directory.clone();
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
            super::legacy::retain_fixture_rows(&mut writer, &directory, geometry)?;
        }
        Ok(writer)
    }
    /// Materialize the selected old implementation through the writer contract.
    pub fn build(self) -> Result<TapeAuthority, StateError> {
        let authority = TapeAuthority::open(&self.directory, self.tape_uuid)?;
        if !self.rows.is_empty() {
            let geometry = self
                .geometry
                .or_else(|| {
                    self.commits.first().and_then(|commit| {
                        commit
                            .scheme
                            .clone()
                            .map(|scheme| (commit.block_size, scheme))
                    })
                })
                .ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "structural fixture rows require parity geometry".into(),
                    )
                })?;
            super::legacy::build_fixture_rows(
                &self.directory,
                self.tape_uuid,
                geometry,
                &self.rows,
            )?;
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
        super::legacy::fixture_evidence_unchanged(&self.authority, &self.bytes)
    }
}

/// Replace the selected fixture authority with unreadable evidence. This
/// expresses refusal behavior; the backend chooses the invalid host encoding.
pub fn damage_authority(authority: TapeAuthority) -> Result<DamagedAuthorityEvidence, StateError> {
    let bytes = super::legacy::damage_fixture_authority(&authority)?;
    Ok(DamagedAuthorityEvidence { authority, bytes })
}

/// Shared capability used to exercise contention without exposing lock files.
pub use super::legacy::SharedFixtureLease;

/// Hold a shared authority lease for a test's contention interval.
pub fn hold_shared(authority: &TapeAuthority) -> Result<SharedFixtureLease, StateError> {
    super::legacy::hold_fixture_shared(authority)
}

/// Observe a publication from a synchronous test transport callback. This probe
/// does not acquire a second lock while the test actor owns its writer lease.
pub fn has_publication(authority: &TapeAuthority) -> bool {
    super::legacy::fixture_has_publication(authority)
}

/// Locate a structural fixture without duplicating the selected backend's naming rule.
pub fn structural_location(
    checkpoint_directory: &Path,
    tape_uuid: [u8; 16],
) -> Result<PathBuf, StateError> {
    let directory = checkpoint_directory.parent().ok_or_else(|| {
        StateError::JournalReplayFailed("checkpoint directory has no parent".into())
    })?;
    Ok(super::structural_path_in(directory, tape_uuid))
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
    let path = super::legacy::fixture_member_path(authority, ".finalizing");
    let bytes = std::fs::read(&path)?;
    Ok(SavedFinalization { path, bytes })
}
/// A deliberately blocked intent-publication seam.
pub struct BlockedFinalization {
    path: PathBuf,
}
impl BlockedFinalization {
    /// Remove the injected obstruction so a retry can publish normally.
    pub fn unblock(self) -> std::io::Result<()> {
        std::fs::remove_dir(self.path)
    }
}
/// Refuse intent publication before its first byte is written.
pub fn block_finalization(authority: &TapeAuthority) -> std::io::Result<BlockedFinalization> {
    let path = super::legacy::fixture_member_path(authority, ".finalizing.new");
    std::fs::create_dir(&path)?;
    Ok(BlockedFinalization { path })
}
/// Opaque matcher for storage fault injection at this authority's commit seam.
pub struct PublicationTarget {
    path: PathBuf,
}
impl PublicationTarget {
    /// Match a file-model event without exposing the backend's filename to the test.
    pub fn matches(&self, event_path: &Path) -> bool {
        self.path == event_path
    }
}
/// Bind a storage fault injector to this authority's commit member.
pub fn publication_target(authority: &TapeAuthority) -> PublicationTarget {
    PublicationTarget {
        path: super::legacy::fixture_member_path(authority, ""),
    }
}
