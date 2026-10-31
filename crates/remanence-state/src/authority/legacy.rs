//! Compile-time B2 authority binding over the existing checkpoint journal.
//! The structural companion is read through the parity-owned frozen-row facet.
use crate::checkpoint as legacy;
#[cfg(any(test, feature = "fixtures"))]
use crate::CheckpointTerminalRecoveryAuthority;
use crate::{
    CheckpointBoundedReplayMetrics, CheckpointJournalRecord, StateError,
    TerminalFinalizationIntent, TerminalFinalizationProgress,
};
use std::path::Path;
/// Compile-time old authority binding for TapeAuthority.
#[derive(Debug)]
pub struct TapeAuthority(pub(crate) legacy::FileCheckpointJournal);
impl TapeAuthority {
    /// Enumerate retained authority entries in the backend's stable order.
    pub fn list_entries(
        directory: impl AsRef<Path>,
    ) -> Result<Vec<std::path::PathBuf>, StateError> {
        legacy::list_checkpoint_journals(directory)
    }
    /// Decode the identity of an enumerated authority entry.
    pub fn entry_tape_uuid(path: &Path) -> Result<[u8; 16], StateError> {
        legacy::tape_uuid_from_checkpoint_path(path)
    }
    /// Exists through the selected authority implementation.
    pub fn exists(dir: impl AsRef<Path>, tape_uuid: [u8; 16]) -> Result<bool, StateError> {
        legacy::FileCheckpointJournal::exists(dir, tape_uuid)
    }
    /// Open through the selected authority implementation.
    pub fn open(dir: impl AsRef<Path>, tape_uuid: [u8; 16]) -> Result<Self, StateError> {
        legacy::FileCheckpointJournal::open(dir, tape_uuid).map(Self)
    }
    /// Open this authority's structural companion using its stored identity.
    pub fn open_structural(
        &self,
        block_size: u32,
        scheme: remanence_parity::ParityScheme,
    ) -> Result<remanence_parity::authority::StructuralCapability, remanence_parity::JournalError>
    {
        remanence_parity::TapeRowAuthority::open(
            structural_path(self.0.path(), self.0.tape_uuid())?,
            self.0.tape_uuid(),
            block_size,
            scheme,
        )
        .map(|rows| Box::new(rows) as remanence_parity::authority::StructuralCapability)
    }
    /// Acquire exclusive through the selected authority implementation.
    pub fn acquire_exclusive(&self) -> Result<TapeAuthorityLease, StateError> {
        self.0.acquire_exclusive().map(TapeAuthorityLease::new)
    }
    /// Acquire exclusive for terminal recovery through the selected authority implementation.
    pub fn acquire_exclusive_for_terminal_recovery(
        &self,
    ) -> Result<TapeAuthorityLease, StateError> {
        self.0
            .acquire_exclusive_for_terminal_recovery()
            .map(TapeAuthorityLease::new)
    }
    /// Append through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn append(&self, record: &CheckpointJournalRecord) -> Result<(), StateError> {
        self.acquire_exclusive()?.publish_report(&record.into())
    }
    /// Replay through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn replay(&self) -> Result<Vec<CheckpointJournalRecord>, StateError> {
        TapeAuthorityView::replay_commits(&mut &self.0)
            .map(|commits| commits.iter().map(Into::into).collect())
    }
    /// Last through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn last(&self) -> Result<Option<CheckpointJournalRecord>, StateError> {
        self.latest_commit()
            .map(|commit| commit.as_ref().map(Into::into))
    }
    /// Path through the selected authority implementation.
    pub(crate) fn path(&self) -> &Path {
        self.0.path()
    }
    /// Terminal finalization intent through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn terminal_finalization_intent(
        &self,
    ) -> Result<Option<TerminalFinalizationIntent>, StateError> {
        self.pending_finalization()
            .map(|intent| intent.as_ref().map(Into::into))
    }
}
/// Compile-time old authority binding for TapeAuthorityLease.
#[derive(Debug)]
pub struct TapeAuthorityLease {
    // Rust drops fields in declaration order: release parity before checkpoint.
    structural: Option<remanence_parity::authority::StructuralCapability>,
    pub(crate) checkpoint: legacy::FileCheckpointJournalLease,
    structural_scope: StructuralScope,
}
#[derive(Debug, PartialEq, Eq)]
enum StructuralScope {
    Idle,
    Lent,
    Retiring,
}
impl TapeAuthorityLease {
    fn new(checkpoint: legacy::FileCheckpointJournalLease) -> Self {
        Self {
            checkpoint,
            structural: None,
            structural_scope: StructuralScope::Idle,
        }
    }
    /// Open the structural side belonging to this leased tape.
    pub fn open_structural(
        &self,
        block_size: u32,
        scheme: remanence_parity::ParityScheme,
    ) -> Result<remanence_parity::authority::StructuralCapability, remanence_parity::JournalError>
    {
        let path = structural_path(self.checkpoint.authority_path(), self.tape_uuid())?;
        remanence_parity::TapeRowAuthority::open(path, self.tape_uuid(), block_size, scheme)
            .map(|rows| Box::new(rows) as remanence_parity::authority::StructuralCapability)
    }
    /// Attach the structural capability opened under this exclusive owner.
    /// Replacement is refused while a publisher is retained or lent.
    pub fn retain_structural_publisher(
        &mut self,
        publisher: impl remanence_parity::authority::StructuralAuthority + 'static,
    ) -> Result<(), StateError> {
        if publisher.tape_uuid() != self.tape_uuid()
            || self.structural.is_some()
            || self.structural_scope != StructuralScope::Idle
        {
            return Err(StateError::JournalReplayFailed(
                "structural publication capability does not belong to this unbound writer".into(),
            ));
        }
        self.structural = Some(Box::new(publisher));
        Ok(())
    }
    /// Lend both old publication phases for one operation. Loans cannot escape
    /// the callback or be nested. The lock survives unwinding except when the
    /// terminal operation has explicitly ended its old publication scope.
    pub fn with_structural<R>(
        &mut self,
        operation: impl FnOnce(
            &mut Self,
            Option<&mut dyn remanence_parity::authority::StructuralAuthority>,
        ) -> R,
    ) -> Result<R, StateError> {
        self.with_structural_scope(|owner, rows| {
            operation(
                owner,
                rows.as_mut().map(|rows| {
                    rows.as_mut() as &mut dyn remanence_parity::authority::StructuralAuthority
                }),
            )
        })
    }
    /// Borrow the owned slot so a terminal operation can release it at its
    /// original boundary, before later catalog work in the same callback.
    pub fn with_structural_scope<R>(
        &mut self,
        operation: impl FnOnce(
            &mut Self,
            &mut Option<remanence_parity::authority::StructuralCapability>,
        ) -> R,
    ) -> Result<R, StateError> {
        if self.structural_scope != StructuralScope::Idle {
            return Err(StateError::JournalReplayFailed(
                "nested structural publication loan".into(),
            ));
        }
        let mut structural = self.structural.take();
        self.structural_scope = StructuralScope::Lent;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            operation(self, &mut structural)
        }));
        let retire = self.structural_scope == StructuralScope::Retiring;
        self.structural_scope = StructuralScope::Idle;
        if !retire {
            self.structural = structural;
        }
        match result {
            Ok(result) => Ok(result),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
    /// End the old terminal publication scope at its original boundary.
    /// An outstanding loan is dropped when the current operation returns.
    pub fn finish_structural_scope(&mut self) {
        self.structural = None;
        if self.structural_scope == StructuralScope::Lent {
            self.structural_scope = StructuralScope::Retiring;
        }
    }
    /// Read the structural facet retained by this writer.
    pub fn structural(&self) -> Option<&dyn remanence_parity::authority::StructuralAuthority> {
        self.structural
            .as_ref()
            .map(|rows| rows.as_ref() as &dyn remanence_parity::authority::StructuralAuthority)
    }
    /// Borrow the retained publication facet for a media operation.
    pub fn structural_mut(
        &mut self,
    ) -> Option<&mut dyn remanence_parity::authority::StructuralAuthority> {
        self.structural
            .as_mut()
            .map(|rows| rows.as_mut() as &mut dyn remanence_parity::authority::StructuralAuthority)
    }
    /// Tape uuid through the selected authority implementation.
    pub fn tape_uuid(&self) -> [u8; 16] {
        self.checkpoint.tape_uuid()
    }
    /// For each record bounded through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn for_each_record_bounded(
        &self,
        mut visitor: impl FnMut(&CheckpointJournalRecord) -> Result<(), StateError>,
    ) -> Result<CheckpointBoundedReplayMetrics, StateError> {
        self.visit_commits(&mut |commit| visitor(&commit.into()))
    }
    /// Last record bounded through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn last_record_bounded(&self) -> Result<Option<CheckpointJournalRecord>, StateError> {
        self.latest_commit()
            .map(|commit| commit.as_ref().map(Into::into))
    }
    /// Bounded replay metrics through the selected authority implementation.
    pub fn bounded_replay_metrics(&self) -> Result<CheckpointBoundedReplayMetrics, StateError> {
        self.visit_commits(&mut |_| Ok(()))
    }
    /// Replay through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn replay(&mut self) -> Result<Vec<CheckpointJournalRecord>, StateError> {
        self.replay_commits()
            .map(|commits| commits.iter().map(Into::into).collect())
    }
    /// Replay for terminal recovery through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn replay_for_terminal_recovery(
        &mut self,
    ) -> Result<CheckpointTerminalRecoveryAuthority, StateError> {
        self.recover_finalization()
            .map(|recovery| CheckpointTerminalRecoveryAuthority {
                records: recovery.commits.iter().map(Into::into).collect(),
                finalization_intent: recovery.finalization.as_ref().map(Into::into),
            })
    }
    /// Begin terminal finalization through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn begin_terminal_finalization(
        &mut self,
        intent: &TerminalFinalizationIntent,
    ) -> Result<TerminalFinalizationIntent, StateError> {
        self.begin_finalization(&intent.into())
            .map(|intent| (&intent).into())
    }
    /// Retire provisional manual finalization through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn retire_provisional_manual_finalization(
        &mut self,
        expected: &TerminalFinalizationIntent,
        catalog_projection_present: bool,
        idempotency_binding_present: bool,
    ) -> Result<(), StateError> {
        self.retire_preparation(
            &expected.into(),
            catalog_projection_present,
            idempotency_binding_present,
        )
    }
    /// Terminal finalization intent through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn terminal_finalization_intent(
        &self,
    ) -> Result<Option<TerminalFinalizationIntent>, StateError> {
        self.pending_finalization()
            .map(|intent| intent.as_ref().map(Into::into))
    }
    /// Advance terminal finalization through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn advance_terminal_finalization(
        &mut self,
        expected: TerminalFinalizationProgress,
        next: TerminalFinalizationProgress,
    ) -> Result<TerminalFinalizationIntent, StateError> {
        self.advance_finalization(expected, next)
            .map(|intent| (&intent).into())
    }
    /// Mark terminal recovery required through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn mark_terminal_recovery_required(
        &mut self,
    ) -> Result<TerminalFinalizationIntent, StateError> {
        self.require_finalization_recovery()
            .map(|intent| (&intent).into())
    }
    /// Append terminal finalization through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn append_terminal_finalization(
        &mut self,
        records: &[CheckpointJournalRecord],
    ) -> Result<(), StateError> {
        self.publish_sealed_reports(&records.iter().map(Into::into).collect::<Vec<_>>(), || {
            Ok(())
        })
    }
    /// Append terminal finalization with after fsync through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn append_terminal_finalization_with_after_fsync(
        &mut self,
        records: &[CheckpointJournalRecord],
        after_fsync: impl FnOnce() -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        self.publish_sealed_reports(
            &records.iter().map(Into::into).collect::<Vec<_>>(),
            after_fsync,
        )
    }
    /// Append through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn append(&mut self, record: &CheckpointJournalRecord) -> Result<(), StateError> {
        self.publish_report(&record.into())
    }
    /// Append batch through the selected authority implementation.
    #[cfg(any(test, feature = "fixtures"))]
    pub fn append_batch(&mut self, records: &[CheckpointJournalRecord]) -> Result<(), StateError> {
        self.publish_reports(&records.iter().map(Into::into).collect::<Vec<_>>())
    }
}

use super::{AuthoritySummary, TapeAuthorityView, TapeAuthorityWriter};
use crate::checkpoint::AuthoritySnapshot;
impl TapeAuthorityView for TapeAuthority {
    type TerminalPrefix = super::AuthorityPrefixPlan;
    type TerminalSource = legacy::CheckpointTerminalIndexRecordSource<'static>;
    fn terminal_source(
        &self,
        structural: Option<&dyn remanence_parity::authority::StructuralAuthority>,
        prefix: remanence_parity::authority::TerminalPrefixMode<&Self::TerminalPrefix>,
    ) -> Result<Self::TerminalSource, StateError> {
        self.0.terminal_source(structural, prefix)
    }

    type Error = StateError;
    type Commit = super::AuthorityCommit;
    type ReplayMetrics = CheckpointBoundedReplayMetrics;
    fn latest_commit(&self) -> Result<Option<Self::Commit>, StateError> {
        self.0.latest_commit()
    }
    fn visit_commits(
        &self,
        visitor: &mut remanence_parity::authority::AuthorityVisitor<'_, Self::Commit, StateError>,
    ) -> Result<Self::ReplayMetrics, StateError> {
        self.0.visit_commits(visitor)
    }
    fn replay_commits(&mut self) -> Result<Vec<Self::Commit>, StateError> {
        self.0.replay_commits()
    }
    fn pending_finalization(&self) -> Result<Option<Self::Finalization>, StateError> {
        self.0.pending_finalization()
    }
    type Finalization = super::AuthorityFinalization;
    type Snapshot = AuthoritySnapshot;
    fn authority_summary(&self) -> Result<AuthoritySummary, StateError> {
        self.0.authority_summary()
    }
    fn validate_append_rows(
        &self,
        rows: &remanence_parity::FrozenFileRows,
    ) -> Result<(), StateError> {
        self.0.validate_append_rows(rows)
    }
    fn freeze(&self) -> Result<AuthoritySnapshot, StateError> {
        self.0.freeze()
    }
    fn finalization_status(&self) -> Result<Option<Self::Finalization>, StateError> {
        self.0.finalization_status()
    }
}
impl TapeAuthorityView for TapeAuthorityLease {
    type TerminalPrefix = super::AuthorityPrefixPlan;
    type TerminalSource = legacy::CheckpointTerminalIndexRecordSource<'static>;
    fn terminal_source(
        &self,
        structural: Option<&dyn remanence_parity::authority::StructuralAuthority>,
        prefix: remanence_parity::authority::TerminalPrefixMode<&Self::TerminalPrefix>,
    ) -> Result<Self::TerminalSource, StateError> {
        self.checkpoint.terminal_source(structural, prefix)
    }

    type Error = StateError;
    type Commit = super::AuthorityCommit;
    type ReplayMetrics = CheckpointBoundedReplayMetrics;
    fn latest_commit(&self) -> Result<Option<Self::Commit>, StateError> {
        self.checkpoint.latest_commit()
    }
    fn visit_commits(
        &self,
        visitor: &mut remanence_parity::authority::AuthorityVisitor<'_, Self::Commit, StateError>,
    ) -> Result<Self::ReplayMetrics, StateError> {
        self.checkpoint.visit_commits(visitor)
    }
    fn replay_commits(&mut self) -> Result<Vec<Self::Commit>, StateError> {
        self.checkpoint.replay_commits()
    }
    fn pending_finalization(&self) -> Result<Option<Self::Finalization>, StateError> {
        self.checkpoint.pending_finalization()
    }
    type Finalization = super::AuthorityFinalization;
    type Snapshot = AuthoritySnapshot;
    fn authority_summary(&self) -> Result<AuthoritySummary, StateError> {
        Ok(self.freeze()?.summary().clone())
    }
    fn validate_append_rows(
        &self,
        rows: &remanence_parity::FrozenFileRows,
    ) -> Result<(), StateError> {
        self.checkpoint.validate_append_rows(rows)
    }
    fn freeze(&self) -> Result<AuthoritySnapshot, StateError> {
        if self.structural_scope != StructuralScope::Idle {
            return Err(StateError::JournalReplayFailed(
                "cannot freeze during a legacy publication operation".into(),
            ));
        }
        self.checkpoint
            .freeze_with_structural(self.structural.as_ref().map(|rows| rows.as_ref()))
    }
    fn finalization_status(&self) -> Result<Option<Self::Finalization>, StateError> {
        Ok(self.freeze()?.finalization().cloned())
    }
}
impl TapeAuthorityWriter for TapeAuthorityLease {
    fn publish_commit(
        &mut self,
        publication: &remanence_parity::authority::FilePublication,
    ) -> Result<remanence_parity::authority::PublicationReceipt, remanence_parity::JournalError>
    {
        self.structural_mut()
            .ok_or_else(|| {
                remanence_parity::JournalError::Codec("writer has no structural capability".into())
            })?
            .publish_commit(publication)
    }
    fn publish_terminal_prefix(
        &mut self,
        prefix: &remanence_parity::authority::FilePublication,
    ) -> Result<remanence_parity::authority::PublicationReceipt, remanence_parity::JournalError>
    {
        self.structural_mut()
            .ok_or_else(|| {
                remanence_parity::JournalError::Codec("writer has no structural capability".into())
            })?
            .publish_terminal_prefix(prefix)
    }
    fn publish_terminal_component(
        &mut self,
        component: &remanence_parity::authority::FilePublication,
    ) -> Result<remanence_parity::authority::PublicationReceipt, remanence_parity::JournalError>
    {
        self.structural_mut()
            .ok_or_else(|| {
                remanence_parity::JournalError::Codec("writer has no structural capability".into())
            })?
            .publish_terminal_component(component)
    }
    type Recovery = super::FinalizationRecovery;
    fn recover_finalization(&mut self) -> Result<Self::Recovery, StateError> {
        self.checkpoint
            .replay_for_terminal_recovery()
            .map(|recovery| super::FinalizationRecovery {
                commits: recovery.records.iter().map(Into::into).collect(),
                finalization: recovery.finalization_intent.as_ref().map(Into::into),
            })
    }
    type Report = super::AuthorityCommit;
    type Progress = TerminalFinalizationProgress;
    fn publish_report(&mut self, report: &Self::Report) -> Result<(), StateError> {
        self.checkpoint.append(&record_for_publication(report)?)
    }
    fn lease_tape_uuid(&self) -> [u8; 16] {
        self.checkpoint.tape_uuid()
    }
    fn publish_reports(&mut self, reports: &[Self::Report]) -> Result<(), StateError> {
        self.checkpoint.append_batch(
            &reports
                .iter()
                .map(record_for_publication)
                .collect::<Result<Vec<_>, _>>()?,
        )
    }
    fn publish_sealed_reports<F: FnOnce() -> Result<(), StateError>>(
        &mut self,
        reports: &[Self::Report],
        after_fsync: F,
    ) -> Result<(), StateError> {
        self.checkpoint
            .append_terminal_finalization_with_after_fsync(
                &reports
                    .iter()
                    .map(record_for_publication)
                    .collect::<Result<Vec<_>, _>>()?,
                after_fsync,
            )
    }
    fn begin_finalization(
        &mut self,
        plan: &Self::Finalization,
    ) -> Result<Self::Finalization, StateError> {
        self.checkpoint
            .begin_terminal_finalization(&plan.into())
            .map(|intent| (&intent).into())
    }
    fn advance_finalization(
        &mut self,
        expected: Self::Progress,
        next: Self::Progress,
    ) -> Result<Self::Finalization, StateError> {
        self.checkpoint
            .advance_terminal_finalization(expected, next)
            .map(|intent| (&intent).into())
    }
    fn require_finalization_recovery(&mut self) -> Result<Self::Finalization, StateError> {
        self.checkpoint
            .mark_terminal_recovery_required()
            .map(|intent| (&intent).into())
    }
    fn retire_preparation(
        &mut self,
        expected: &Self::Finalization,
        catalog_present: bool,
        binding_present: bool,
    ) -> Result<(), StateError> {
        self.checkpoint.retire_provisional_manual_finalization(
            &expected.into(),
            catalog_present,
            binding_present,
        )
    }
}

/// Old fixture storage expansion; semantic builders never name either journal file.
#[cfg(any(test, feature = "fixtures"))]
pub(super) fn build_fixture_rows(
    directory: &Path,
    tape_uuid: [u8; 16],
    geometry: (u32, remanence_parity::ParityScheme),
    reports: &[remanence_parity::authority::ShadowPublication],
) -> Result<(), StateError> {
    use remanence_parity::TapeReportPublisher;
    let (block_size, scheme) = geometry;
    let parent = directory.parent().ok_or_else(|| {
        StateError::JournalReplayFailed("fixture authority directory has no parent".into())
    })?;
    let mut rows = remanence_parity::authority::fixtures::RowAuthorityBuilder::new(
        parent, tape_uuid, block_size, scheme,
    )
    .build()
    .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
    for report in reports {
        rows.publish_fixture(report)
            .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
    }
    Ok(())
}

#[cfg(any(test, feature = "fixtures"))]
pub(super) fn retain_fixture_rows(
    writer: &mut TapeAuthorityLease,
    directory: &Path,
    geometry: (u32, remanence_parity::ParityScheme),
) -> Result<(), StateError> {
    let parent = directory.parent().ok_or_else(|| {
        StateError::JournalReplayFailed("fixture authority directory has no parent".into())
    })?;
    let rows = remanence_parity::authority::fixtures::RowAuthorityBuilder::new(
        parent,
        writer.tape_uuid(),
        geometry.0,
        geometry.1,
    )
    .build()
    .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
    writer.retain_structural_publisher(rows)
}

use remanence_parity::ParityError;
/// Why the combined durable authorities refused a resume.
#[derive(Debug)]
pub enum AuthorityAgreementError {
    /// The commit records are missing, conflict, or are incomplete or
    /// ambiguous (REM-PARITY 3.4): reported as `ResumeAppend` with its detail.
    Records(ParityError),
    /// An operational failure reading the checkpoint journal, which keeps
    /// its own error, as `resume_record_result` does for the parity journal.
    State(StateError),
}

impl From<StateError> for AuthorityAgreementError {
    fn from(error: StateError) -> Self {
        match error {
            StateError::JournalReplayFailed(ref detail)
                if detail == "parity journal has a committed prefix but checkpoint authority is empty" =>
                Self::Records(ParityError::ResumeAppend(detail.clone())),
            StateError::JournalReplayFailed(_) => Self::Records(ParityError::ResumeAppend(error.to_string())),
            other => Self::State(other),
        }
    }
}

impl TapeAuthorityLease {
    /// Validate the existing structural/checkpoint agreement at the resume boundary.
    pub fn validate_resume_rows(
        &self,
        parity: &dyn remanence_parity::authority::StructuralAuthority,
        checkpoint_is_empty: bool,
        committed_tape_file_count: u64,
    ) -> Result<(), AuthorityAgreementError> {
        if checkpoint_is_empty {
            if committed_tape_file_count != 0 {
                return Err(AuthorityAgreementError::Records(ParityError::ResumeAppend(
                    "parity journal has a committed prefix but checkpoint authority is empty"
                        .into(),
                )));
            }
        } else {
            super::TerminalIndexSource::new_replay_backed(self, parity).map_err(
                |error| match error {
                    StateError::JournalReplayFailed(_) => AuthorityAgreementError::Records(
                        ParityError::ResumeAppend(error.to_string()),
                    ),
                    other => AuthorityAgreementError::State(other),
                },
            )?;
        }
        Ok(())
    }
}

#[cfg(any(test, feature = "fixtures"))]
pub(super) fn damage_fixture_authority(authority: &TapeAuthority) -> Result<Vec<u8>, StateError> {
    let bytes = b"legacy-or-torn-checkpoint".to_vec();
    std::fs::write(authority.path(), &bytes)
        .map_err(|error| StateError::io_at("damage authority fixture", authority.path(), error))?;
    Ok(bytes)
}
#[cfg(any(test, feature = "fixtures"))]
pub(super) fn fixture_evidence_unchanged(
    authority: &TapeAuthority,
    bytes: &[u8],
) -> Result<bool, StateError> {
    std::fs::read(authority.path())
        .map(|actual| actual == bytes)
        .map_err(|error| {
            StateError::io_at("read authority fixture evidence", authority.path(), error)
        })
}

/// Opaque shared fixture capability; the backend owns its lock-file protocol.
#[cfg(any(test, feature = "fixtures"))]
pub struct SharedFixtureLease {
    _guard: nix::fcntl::Flock<remanence_library::durable_file::File>,
}

#[cfg(any(test, feature = "fixtures"))]
pub(super) fn hold_fixture_shared(
    authority: &TapeAuthority,
) -> Result<SharedFixtureLease, StateError> {
    Ok(SharedFixtureLease {
        _guard: authority.0.fixture_shared_lock()?,
    })
}

#[cfg(any(test, feature = "fixtures"))]
pub(super) fn fixture_has_publication(authority: &TapeAuthority) -> bool {
    authority.0.fixture_has_publication()
}

/// The sole rule for locating the old structural companion.
pub(crate) fn structural_path(
    checkpoint_path: &Path,
    tape_uuid: [u8; 16],
) -> Result<std::path::PathBuf, remanence_parity::JournalError> {
    let directory = checkpoint_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| {
            remanence_parity::JournalError::Codec(
                "checkpoint authority has no journal directory".into(),
            )
        })?;
    Ok(structural_path_in(directory, tape_uuid))
}

impl From<&CheckpointJournalRecord> for super::AuthorityCommit {
    fn from(record: &CheckpointJournalRecord) -> Self {
        let last_files = record
            .barrier_bundle
            .as_ref()
            .or_else(|| record.object_tape_file_bundles.last());
        let protected = last_files.map_or(0, |unit| unit.highest_protected_ordinal);
        let total = last_files.map_or_else(
            || {
                record
                    .objects
                    .last()
                    .map_or(0, |object| object.total_committed_ordinals)
            },
            |unit| unit.total_committed_ordinals,
        );
        Self {
            protected,
            total,
            ordinal: record.ordinal,
            object_count: record.committed_object_count,
            cursor: remanence_parity::CommitCursor {
                partition: record.eod_partition,
                lba: record.eod_lba,
            },
            tape_uuid: record.tape_uuid,
            operation_id: record.batch_id,
            next_file: record.next_tape_file_number,
            block_size: record.block_size,
            payloads: record
                .objects
                .iter()
                .map(|object| super::ObjectPayload {
                    projection: object.into(),
                    files: object.files.clone(),
                })
                .collect(),
            scheme: record.scheme.clone(),
            object_files: record
                .object_tape_file_bundles
                .iter()
                .map(Into::into)
                .collect(),
            barrier: record.barrier_bundle.as_ref().map(Into::into),
            finalization: record.terminal_finalization.as_ref().map(Into::into),
            sealed: record.sealed_after_write,
            seal_due: record.seal_due,
            check: None,
        }
    }
}
impl From<&super::AuthorityCommit> for CheckpointJournalRecord {
    fn from(commit: &super::AuthorityCommit) -> Self {
        Self {
            ordinal: commit.ordinal,
            committed_object_count: commit.object_count,
            eod_partition: commit.cursor.partition,
            eod_lba: commit.cursor.lba,
            tape_uuid: commit.tape_uuid,
            batch_id: commit.operation_id,
            next_tape_file_number: commit.next_file,
            block_size: commit.block_size,
            objects: commit
                .payloads
                .iter()
                .map(|payload| crate::CheckpointObjectProjection {
                    object: payload.projection.object.clone(),
                    files: payload.files.clone(),
                    copy: payload.projection.copy.clone(),
                    block_size: payload.projection.block_size,
                    block_count: payload.projection.block_count,
                    fresh_tape: payload.projection.fresh_tape,
                    total_committed_ordinals: payload.projection.total_committed_ordinals,
                    object_recovery_row: legacy_recovery_row(
                        &payload.projection.object_recovery_row,
                    ),
                })
                .collect(),
            scheme: commit.scheme.clone(),
            object_tape_file_bundles: commit.object_files.iter().map(Into::into).collect(),
            barrier_bundle: commit.barrier.as_ref().map(Into::into),
            terminal_finalization: commit.finalization.as_ref().map(Into::into),
            sealed_after_write: commit.sealed,
            seal_due: commit.seal_due,
        }
    }
}

fn legacy_recovery_row(
    row: &remanence_parity::ObjectRecoveryRow,
) -> crate::CheckpointObjectRecoveryRow {
    use crate::CheckpointObjectRecoveryRepresentation as Old;
    use remanence_parity::ObjectRecoveryRepresentation as Neutral;
    crate::CheckpointObjectRecoveryRow {
        tape_file_number: row.tape_file_number,
        stored_block_count: row.stored_block_count,
        object_id: row.object_id.clone().unwrap_or_default(),
        representation: match &row.representation {
            Neutral::Plaintext {
                manifest_first_chunk_lba,
                manifest_size_bytes,
                manifest_chunk_count,
                manifest_sha256,
            } => Old::Plaintext {
                manifest_first_chunk_lba: *manifest_first_chunk_lba,
                manifest_size_bytes: *manifest_size_bytes,
                manifest_chunk_count: *manifest_chunk_count,
                manifest_sha256: *manifest_sha256,
            },
            Neutral::Encrypted {
                recipient_epoch_ids,
                metadata_frame_len,
                key_frame_len,
            } => Old::Encrypted {
                recipient_epoch_ids: recipient_epoch_ids.clone(),
                metadata_frame_len: *metadata_frame_len,
                key_frame_len: *key_frame_len,
            },
        },
    }
}

pub(crate) fn structural_path_in(directory: &Path, tape_uuid: [u8; 16]) -> std::path::PathBuf {
    remanence_parity::TapeRowAuthority::location_in(directory, tape_uuid)
}

use crate::{
    CatalogIndex, RebuildTapeJournalInput, TapeJournalIndexInput, TapeJournalIngestionOutcome,
};
use remanence_parity::{ParityScheme, TapeRowAuthority};
use std::fs;
pub(crate) fn ingest_structural(
    index: &mut CatalogIndex,
    directory: &Path,
    tape_uuid: [u8; 16],
    block_size: u32,
    scheme: ParityScheme,
) -> Result<TapeJournalIngestionOutcome, StateError> {
    let path = structural_path_in(directory, tape_uuid);
    let reader = match TapeRowAuthority::open_shared_for_replay(
        &path,
        tape_uuid,
        block_size,
        scheme.clone(),
    ) {
        Ok(reader) => reader,
        Err(err) if err.is_lock_contended() => {
            let report =
                index.mark_tape_journal_ingestion_pending(tape_uuid, block_size, &scheme)?;
            return Ok(TapeJournalIngestionOutcome::Pending(report));
        }
        Err(err) => {
            return Err(StateError::JournalReplayFailed(format!(
                "open shared journal replay {}: {err}",
                path.display()
            )));
        }
    };

    let state = reader.load_committed().map_err(|err| {
        StateError::JournalReplayFailed(format!("load committed journal {}: {err}", path.display()))
    })?;
    if !state.orphaned_bundles.is_empty() {
        tracing::warn!(
            tape_uuid = %uuid::Uuid::from_bytes(tape_uuid),
            orphaned_bundle_count = state.orphaned_bundles.len(),
            "ignored sink-journal bundles beyond the last checkpoint watermark"
        );
    }
    let journal_offset_bytes = fs::metadata(&path)
        .map_err(|err| StateError::io_at("stat ingested journal", &path, err))?
        .len();
    let report = index.index_structural_authority(
        TapeJournalIndexInput {
            tape_uuid,
            block_size,
            scheme: Some(scheme),
            journal_offset_bytes,
        },
        &(&state).into(),
    )?;
    Ok(TapeJournalIngestionOutcome::Indexed(report))
}
pub(crate) fn rebuild_structural(
    directory: &Path,
) -> Result<Vec<RebuildTapeJournalInput>, StateError> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|err| StateError::io_at("read journal directory", directory, err))?
    {
        let entry = entry
            .map_err(|err| StateError::io_at("read journal directory entry", directory, err))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("remjournal") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut inputs = Vec::with_capacity(paths.len());
    for path in paths {
        let reader = match TapeRowAuthority::open_shared_existing_for_replay(&path) {
            Ok(reader) => reader,
            Err(err) if err.is_lock_contended() => {
                return Err(StateError::IndexRebuildInProgress);
            }
            Err(err) => {
                return Err(StateError::JournalReplayFailed(format!(
                    "open shared journal replay {}: {err}",
                    path.display()
                )));
            }
        };
        let journal_offset_bytes = fs::metadata(&path)
            .map_err(|err| StateError::io_at("stat ingested journal", &path, err))?
            .len();
        let input = TapeJournalIndexInput {
            tape_uuid: reader.tape_uuid(),
            block_size: reader.block_size(),
            scheme: Some(reader.scheme().clone()),
            journal_offset_bytes,
        };
        let state = reader.load_committed().map_err(|err| {
            StateError::JournalReplayFailed(format!(
                "load committed journal {}: {err}",
                path.display()
            ))
        })?;
        if !state.orphaned_bundles.is_empty() {
            tracing::warn!(
                tape_uuid = %uuid::Uuid::from_bytes(input.tape_uuid),
                orphaned_bundle_count = state.orphaned_bundles.len(),
                "ignored sink-journal bundles beyond the last checkpoint watermark during rebuild"
            );
        }
        inputs.push(RebuildTapeJournalInput {
            input,
            state: (&state).into(),
        });
    }
    Ok(inputs)
}

fn record_for_publication(
    report: &super::AuthorityCommit,
) -> Result<CheckpointJournalRecord, StateError> {
    let record = CheckpointJournalRecord::from(report);
    let decoded = super::AuthorityCommit::from(&record);
    if (report.protected, report.total) != (decoded.protected, decoded.total) {
        return Err(StateError::JournalReplayFailed(
            "publication counters disagree with files and catalog payloads".into(),
        ));
    }
    Ok(record)
}

impl From<CheckpointJournalRecord> for super::AuthorityCommit {
    fn from(record: CheckpointJournalRecord) -> Self {
        Self::from(&record)
    }
}

#[cfg(any(test, feature = "fixtures"))]
pub(super) fn fixture_member_path(authority: &TapeAuthority, suffix: &str) -> std::path::PathBuf {
    let mut path = authority.path().as_os_str().to_os_string();
    path.push(suffix);
    path.into()
}

impl From<&TerminalFinalizationIntent> for super::AuthorityFinalization {
    fn from(value: &TerminalFinalizationIntent) -> Self {
        Self {
            tape_uuid: value.tape_uuid,
            trigger: value.trigger,
            manual: value.manual.clone(),
            progress: value.progress,
            recovery_required: value.recovery_required,
            edition_id: value.edition_id,
            edition_sequence: value.edition_sequence,
            edition_digest: value.edition_digest,
            writer_version: value.writer_version.clone(),
            write_timestamp: value.write_timestamp.clone(),
            layout: value.layout.clone(),
            terminal_prefix: value.terminal_prefix.as_ref().map(Into::into),
        }
    }
}

impl From<&super::AuthorityFinalization> for TerminalFinalizationIntent {
    fn from(value: &super::AuthorityFinalization) -> Self {
        Self {
            tape_uuid: value.tape_uuid,
            trigger: value.trigger,
            manual: value.manual.clone(),
            progress: value.progress,
            recovery_required: value.recovery_required,
            edition_id: value.edition_id,
            edition_sequence: value.edition_sequence,
            edition_digest: value.edition_digest,
            writer_version: value.writer_version.clone(),
            write_timestamp: value.write_timestamp.clone(),
            layout: value.layout.clone(),
            terminal_prefix: value.terminal_prefix.as_ref().map(Into::into),
        }
    }
}

impl From<&crate::TerminalFinalizationPrefixPlan> for super::AuthorityPrefixPlan {
    fn from(value: &crate::TerminalFinalizationPrefixPlan) -> Self {
        Self {
            diagnostics: value.diagnostics.clone(),
            start_tape_file_number: value.start_tape_file_number,
            tail_start_tape_file_number: value.tail_start_tape_file_number,
            start_lba: value.start_lba,
            tail_start_lba: value.tail_start_lba,
            parity_map_tape_file_number: value.parity_map_tape_file_number,
            sidecar_directory_entries: value
                .sidecar_directory_entries
                .iter()
                .map(Into::into)
                .collect(),
            publication: (&value.committed_bundle).into(),
        }
    }
}

impl From<&super::AuthorityPrefixPlan> for crate::TerminalFinalizationPrefixPlan {
    fn from(value: &super::AuthorityPrefixPlan) -> Self {
        Self {
            diagnostics: value.diagnostics.clone(),
            start_tape_file_number: value.start_tape_file_number,
            tail_start_tape_file_number: value.tail_start_tape_file_number,
            start_lba: value.start_lba,
            tail_start_lba: value.tail_start_lba,
            parity_map_tape_file_number: value.parity_map_tape_file_number,
            sidecar_directory_entries: value
                .sidecar_directory_entries
                .iter()
                .map(Into::into)
                .collect(),
            committed_bundle: (&value.publication).into(),
        }
    }
}

impl TryFrom<&super::AuthorityPrefixPlan> for remanence_parity::TerminalPrefixPlan {
    type Error = StateError;
    fn try_from(plan: &super::AuthorityPrefixPlan) -> Result<Self, StateError> {
        Self::try_from(&crate::TerminalFinalizationPrefixPlan::from(plan))
    }
}
impl From<&remanence_parity::TerminalPrefixPlan> for super::AuthorityPrefixPlan {
    fn from(plan: &remanence_parity::TerminalPrefixPlan) -> Self {
        Self::from(&crate::TerminalFinalizationPrefixPlan::from(plan))
    }
}
#[cfg(any(test, feature = "fixtures"))]
impl CatalogIndex {
    /// Old-codec fixture adapter; production catalog projection is neutral.
    pub fn project_checkpoint_record(
        &mut self,
        record: &CheckpointJournalRecord,
    ) -> Result<(), StateError> {
        self.project_authority_commit(&record.into())
    }
}

impl From<&crate::CheckpointObjectProjection>
    for crate::tape_log::payload::CommittedObjectProjection
{
    fn from(v: &crate::CheckpointObjectProjection) -> Self {
        Self {
            object: v.object.clone(),
            copy: v.copy.clone(),
            block_size: v.block_size,
            block_count: v.block_count,
            fresh_tape: v.fresh_tape,
            total_committed_ordinals: v.total_committed_ordinals,
            object_recovery_row: v.object_recovery_row.to_parity_row(),
        }
    }
}

#[cfg(any(test, feature = "fixtures"))]
impl crate::CatalogIndex {
    /// Adapter for isolated legacy projection fixtures.
    pub fn index_committed_tape_journal(
        &mut self,
        input: crate::TapeJournalIndexInput,
        state: &remanence_parity::CommittedState,
    ) -> Result<crate::TapeJournalIndexReport, StateError> {
        self.index_structural_authority(input, &state.into())
    }
    /// Adapter for isolated legacy projection fixtures.
    pub fn project_committed_tape_file_bundle(
        &mut self,
        input: crate::TapeJournalIndexInput,
        bundle: &remanence_parity::CommittedBundle,
    ) -> Result<crate::TapeJournalIndexReport, StateError> {
        self.project_file_publication(input, &bundle.into())
    }
}

impl From<&crate::CheckpointObjectProjection> for super::ObjectPayload {
    fn from(object: &crate::CheckpointObjectProjection) -> Self {
        Self {
            projection: object.into(),
            files: object.files.clone(),
        }
    }
}
