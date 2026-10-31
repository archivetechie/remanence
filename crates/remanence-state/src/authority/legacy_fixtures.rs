//! Semantic authority fixtures. Tests supply commits, rows and immutable plans;
//! only the selected backend knows their file names and host encodings.
use super::legacy::TapeAuthority;
use super::TapeAuthorityWriter;
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
    pub fn build_lease(self) -> Result<super::legacy::TapeAuthorityLease, StateError> {
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
