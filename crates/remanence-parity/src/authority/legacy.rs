//! Sole B2 structural storage binding. All concrete journal access is confined here.
use super::{FileRowCursor, FrozenFileRows};
use crate::journal as legacy;
use crate::journal::TapeFileJournal;
use crate::{CommittedBundle, CommittedState, JournalError, ParityScheme, TapeReportPublisher};
use std::path::Path;
/// Compile-time old authority binding for TapeRowAuthority.
#[derive(Debug)]
pub struct TapeRowAuthority(pub(crate) legacy::FileTapeFileJournal);
impl TapeRowAuthority {
    #[cfg(any(test, feature = "fixtures"))]
    pub(super) fn open_fixture(
        path: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<Self, JournalError> {
        legacy::FileTapeFileJournal::open_fixture(path.as_ref(), tape_uuid, block_size, scheme)
            .map(Self)
    }

    #[cfg(any(test, feature = "fixtures"))]
    pub(super) fn append_fixture_claims(
        &mut self,
        claims: &[CommittedBundle],
        absent_block_counts: &[(usize, usize)],
    ) -> Result<(), JournalError> {
        self.0.append_fixture_claims(claims, absent_block_counts)
    }
    #[cfg(any(test, feature = "fixtures"))]
    pub(super) fn inject_fixture_fault(
        &self,
        fault: super::fixtures::RowFixtureFault,
    ) -> Result<(), JournalError> {
        self.0.inject_fixture_fault(fault)
    }

    #[cfg(test)]
    pub(crate) fn published_units_for_test(&self) -> Result<Vec<CommittedBundle>, JournalError> {
        self.0.published_units_for_test()
    }

    /// Backend-owned structural location; not a business-code path API.
    #[doc(hidden)]
    pub fn location_in(directory: &Path, tape_uuid: [u8; 16]) -> std::path::PathBuf {
        let name: String = tape_uuid.iter().map(|byte| format!("{byte:02x}")).collect();
        directory.join(format!("{name}.remjournal"))
    }
    /// Open through the selected authority implementation.
    #[doc(hidden)]
    pub fn open(
        path: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<Self, JournalError> {
        legacy::FileTapeFileJournal::open(path, tape_uuid, block_size, scheme).map(Self)
    }
    /// Terminal prefix transition is durable through the selected authority implementation.
    #[cfg(test)]
    pub(crate) fn terminal_prefix_transition_is_durable(
        &self,
        prefix: &CommittedBundle,
        checkpoint: &CommittedBundle,
    ) -> Result<bool, JournalError> {
        self.0
            .terminal_prefix_transition_is_durable(prefix, checkpoint)
    }
    /// Terminal component authority relation through the selected authority implementation.
    #[cfg(test)]
    pub(crate) fn terminal_component_authority_relation(
        &self,
        checkpoint_component_count: u8,
        expected_transitions: &[(CommittedBundle, CommittedBundle)],
    ) -> Result<crate::TerminalComponentAuthorityRelation, JournalError> {
        self.0
            .terminal_component_authority_relation(checkpoint_component_count, expected_transitions)
    }
    /// Open shared for replay through the selected authority implementation.
    #[doc(hidden)]
    pub fn open_shared_for_replay(
        path: impl AsRef<Path>,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<TapeRowReader, JournalError> {
        legacy::FileTapeFileJournal::open_shared_for_replay(path, tape_uuid, block_size, scheme)
            .map(TapeRowReader)
    }
    /// Open shared existing for replay through the selected authority implementation.
    #[doc(hidden)]
    pub fn open_shared_existing_for_replay(
        path: impl AsRef<Path>,
    ) -> Result<TapeRowReader, JournalError> {
        legacy::FileTapeFileJournal::open_shared_existing_for_replay(path).map(TapeRowReader)
    }
    /// Orphaned bundles preserved on open through the selected authority implementation.
    #[cfg(test)]
    pub(crate) fn orphaned_bundles_preserved_on_open(&self) -> usize {
        self.0.orphaned_bundles_preserved_on_open()
    }
    /// Tape uuid through the selected authority implementation.
    pub fn tape_uuid(&self) -> [u8; 16] {
        self.0.tape_uuid()
    }
    /// Path through the selected authority implementation.
    #[doc(hidden)]
    pub fn path(&self) -> &Path {
        self.0.path()
    }
    /// Block size through the selected authority implementation.
    pub fn block_size(&self) -> u32 {
        self.0.block_size()
    }
    /// Drive compression through the selected authority implementation.
    pub fn drive_compression(&self) -> bool {
        self.0.drive_compression()
    }
    /// Scheme through the selected authority implementation.
    pub fn scheme(&self) -> &ParityScheme {
        self.0.scheme()
    }
    /// Replay committed entries bounded through the selected authority implementation.
    pub fn replay_committed_entries_bounded(&self) -> Result<Box<dyn FileRowCursor>, JournalError> {
        self.0.replay_committed_entries_bounded()
    }
    /// Committed snapshot bounded through the selected authority implementation.
    pub fn committed_snapshot_bounded(&self) -> Result<FrozenFileRows, JournalError> {
        self.0.committed_snapshot_bounded()
    }
    /// Planned terminal prefix base snapshot bounded through the selected authority implementation.
    #[doc(hidden)]
    pub fn planned_terminal_prefix_base_snapshot_bounded(
        &self,
        prefix: &CommittedBundle,
    ) -> Result<FrozenFileRows, JournalError> {
        self.0.planned_terminal_prefix_base_snapshot_bounded(prefix)
    }
    /// Terminal prefix snapshot bounded through the selected authority implementation.
    pub fn terminal_prefix_snapshot_bounded(&self) -> Result<FrozenFileRows, JournalError> {
        self.0.terminal_prefix_snapshot_bounded()
    }
    /// Truncate physically discarded suffix through the selected authority implementation.
    #[cfg(test)]
    pub(crate) fn truncate_physically_discarded_suffix(
        &mut self,
        next_file_number: u64,
    ) -> Result<(), JournalError> {
        self.0
            .truncate_physically_discarded_suffix(next_file_number)
    }
    /// Truncate reconciled orphans through the selected authority implementation.
    #[cfg(test)]
    pub(crate) fn truncate_reconciled_orphans(
        &mut self,
        expected_orphans: &[CommittedBundle],
    ) -> Result<CommittedState, JournalError> {
        self.0.truncate_reconciled_orphans(expected_orphans)
    }
}
/// Compile-time old authority binding for TapeRowReader.
#[derive(Debug)]
pub struct TapeRowReader(pub(crate) legacy::FileTapeFileJournalReader);
impl TapeRowReader {
    /// Freeze a shared inspection view, refusing orphan or corrupt authority.
    pub fn frozen_rows(&self) -> Result<FrozenFileRows, JournalError> {
        self.0.frozen_rows()
    }

    /// Tape uuid through the selected authority implementation.
    pub fn tape_uuid(&self) -> [u8; 16] {
        self.0.tape_uuid()
    }
    /// Path through the selected authority implementation.
    #[doc(hidden)]
    pub fn path(&self) -> &Path {
        self.0.path()
    }
    /// Block size through the selected authority implementation.
    pub fn block_size(&self) -> u32 {
        self.0.block_size()
    }
    /// Drive compression through the selected authority implementation.
    pub fn drive_compression(&self) -> bool {
        self.0.drive_compression()
    }
    /// Scheme through the selected authority implementation.
    pub fn scheme(&self) -> &ParityScheme {
        self.0.scheme()
    }
    /// Load committed through the selected authority implementation.
    #[doc(hidden)]
    pub fn load_committed(self) -> Result<CommittedState, JournalError> {
        self.0.load_committed()
    }
}
impl TapeReportPublisher for TapeRowAuthority {
    fn tape_uuid(&self) -> [u8; 16] {
        TapeFileJournal::tape_uuid(&self.0)
    }
    #[cfg(any(test, feature = "fixtures"))]
    fn publish_fixture(&mut self, report: &super::ShadowPublication) -> Result<(), JournalError> {
        match report.bundles.as_slice() {
            [bundle, checkpoint]
                if bundle.kind == crate::CommittedBundleKind::TerminalPrefix
                    && checkpoint.kind == crate::CommittedBundleKind::CheckpointedThrough =>
            {
                self.0
                    .commit_terminal_prefix_transition(bundle, checkpoint)
                    .map(|_| ())
            }
            [bundle, checkpoint]
                if bundle.kind == crate::CommittedBundleKind::TerminalComponent
                    && checkpoint.kind == crate::CommittedBundleKind::CheckpointedThrough =>
            {
                self.0
                    .commit_terminal_component_transition(bundle, checkpoint)
                    .map(|_| ())
            }
            bundles => {
                for bundle in bundles {
                    self.0.commit_bundle(bundle)?;
                }
                Ok(())
            }
        }
    }
    fn publish_commit(
        &mut self,
        publication: &super::FilePublication,
    ) -> Result<super::PublicationReceipt, JournalError> {
        if matches!(
            publication.purpose,
            super::FilePurpose::TerminalPrefix | super::FilePurpose::TerminalComponent
        ) {
            return Err(JournalError::Codec(
                "terminal publication requires its explicit operation".into(),
            ));
        }
        self.0.commit_bundle(&publication.into())?;
        Ok(super::PublicationReceipt::appended(publication.clone()))
    }
    fn publish_terminal_prefix(
        &mut self,
        prefix: &super::FilePublication,
    ) -> Result<super::PublicationReceipt, JournalError> {
        super::validate_terminal_publication(prefix, super::FilePurpose::TerminalPrefix)?;
        let disposition = self
            .0
            .commit_terminal_prefix_transition(&prefix.into(), &terminal_barrier(prefix))?;
        Ok(super::terminal_receipt(prefix, disposition))
    }
    fn publish_terminal_component(
        &mut self,
        component: &super::FilePublication,
    ) -> Result<super::PublicationReceipt, JournalError> {
        super::validate_terminal_publication(component, super::FilePurpose::TerminalComponent)?;
        let disposition = self.0.commit_terminal_component_transition(
            &component.into(),
            &terminal_barrier(component),
        )?;
        Ok(super::terminal_receipt(component, disposition))
    }
    #[cfg(any(test, feature = "fixtures"))]
    fn load_committed(&self) -> Result<CommittedState, JournalError> {
        self.0.load_committed()
    }
    fn session_resume_snapshot(&self) -> Result<Option<FrozenFileRows>, JournalError> {
        self.0.session_resume_snapshot()
    }
    fn session_terminal_resume_snapshot(
        &self,
        prefix: &super::FilePublication,
    ) -> Result<Option<FrozenFileRows>, JournalError> {
        self.0.session_terminal_resume_snapshot(&prefix.into())
    }
    fn committed_snapshot_bounded_authority(&self) -> Result<FrozenFileRows, JournalError> {
        self.0.committed_snapshot_bounded_authority()
    }
}

/// Fixture evidence temporarily absent from the selected authority location.
#[cfg(any(test, feature = "fixtures"))]
pub struct HiddenRowAuthority {
    original: std::path::PathBuf,
    saved: std::path::PathBuf,
}
#[cfg(any(test, feature = "fixtures"))]
impl HiddenRowAuthority {
    /// Restore the exact saved evidence after a missing-authority assertion.
    pub fn restore(self) -> Result<(), JournalError> {
        std::fs::rename(self.saved, self.original)?;
        Ok(())
    }
}
#[cfg(any(test, feature = "fixtures"))]
pub(super) fn hide_fixture_rows(location: &Path) -> Result<HiddenRowAuthority, JournalError> {
    let saved = location.with_extension("saved");
    std::fs::rename(location, &saved)?;
    Ok(HiddenRowAuthority {
        original: location.into(),
        saved,
    })
}

// Only the old implementation maps physical purposes to legacy codec tags.
impl TryFrom<&CommittedBundle> for super::FilePublication {
    type Error = JournalError;
    fn try_from(bundle: &CommittedBundle) -> Result<Self, Self::Error> {
        use super::FilePurpose as New;
        use crate::CommittedBundleKind as Old;
        Ok(Self {
            purpose: match bundle.kind {
                Old::BotBootstrap => New::Bootstrap,
                Old::Object => New::Object,
                Old::ResumeSidecars => New::Sidecars(super::SidecarProvenance::Reconstructed),
                Old::CheckpointSidecars => New::Sidecars(super::SidecarProvenance::Fresh),
                Old::CheckpointedThrough => {
                    return Err(JournalError::UnsupportedOperation(
                        "legacy watermark is not a physical file publication",
                    ))
                }
                Old::TerminalPrefix => New::TerminalPrefix,
                Old::TerminalComponent => New::TerminalComponent,
            },
            files: bundle.entries.clone(),
            protected: bundle.highest_protected_ordinal,
            total: bundle.total_committed_ordinals,
            cursor: None,
            check: None,
            seal_due: None,
        })
    }
}
impl From<&super::FilePublication> for CommittedBundle {
    fn from(publication: &super::FilePublication) -> Self {
        use super::FilePurpose as New;
        use crate::CommittedBundleKind as Old;
        Self {
            kind: match publication.purpose {
                New::Bootstrap => Old::BotBootstrap,
                New::Object => Old::Object,
                New::Sidecars(super::SidecarProvenance::Reconstructed) => Old::ResumeSidecars,
                New::Sidecars(super::SidecarProvenance::Fresh) => Old::CheckpointSidecars,
                New::TerminalPrefix => Old::TerminalPrefix,
                New::TerminalComponent => Old::TerminalComponent,
            },
            entries: publication.files.clone(),
            highest_protected_ordinal: publication.protected,
            total_committed_ordinals: publication.total,
        }
    }
}

#[cfg(any(test, feature = "fixtures"))]
pub(super) fn open_shared_fixture(
    path: &Path,
    tape_uuid: [u8; 16],
    block_size: u32,
    scheme: ParityScheme,
) -> Result<TapeRowReader, JournalError> {
    legacy::FileTapeFileJournal::open_shared_fixture(path, tape_uuid, block_size, scheme)
        .map(TapeRowReader)
}

impl super::StructuralAuthority for TapeRowAuthority {
    #[cfg(any(test, feature = "fixtures"))]
    fn fixture_publications(&self) -> Result<Vec<super::FilePublication>, JournalError> {
        self.0.published_units_for_test().map(|units| {
            units
                .iter()
                .filter_map(|b| super::FilePublication::try_from(b).ok())
                .collect()
        })
    }

    fn freeze(&self) -> Result<FrozenFileRows, JournalError> {
        self.0.committed_snapshot_bounded()
    }
    fn freeze_before_terminal_prefix(
        &self,
        prefix: &super::FilePublication,
    ) -> Result<FrozenFileRows, JournalError> {
        self.0
            .planned_terminal_prefix_base_snapshot_bounded(&prefix.into())
    }
    fn freeze_terminal_prefix(&self) -> Result<FrozenFileRows, JournalError> {
        self.0.terminal_prefix_snapshot_bounded()
    }
    fn prefix_is_durable(&self, prefix: &super::FilePublication) -> Result<bool, JournalError> {
        self.0
            .terminal_prefix_transition_is_durable(&prefix.into(), &terminal_barrier(prefix))
    }
    fn component_relation(
        &self,
        completed: u8,
        expected: &[super::FilePublication],
    ) -> Result<crate::TerminalComponentAuthorityRelation, JournalError> {
        self.0.terminal_component_authority_relation(
            completed,
            &expected
                .iter()
                .map(|component| (component.into(), terminal_barrier(component)))
                .collect::<Vec<_>>(),
        )
    }
    fn pending_count(&self) -> usize {
        self.0.orphaned_bundles_preserved_on_open()
    }
    fn discard_suffix(&mut self, next_file: u64) -> Result<(), JournalError> {
        self.0.truncate_physically_discarded_suffix(next_file)
    }
    fn discard_pending(&mut self, expected: &[super::FilePublication]) -> Result<(), JournalError> {
        self.0
            .truncate_reconciled_orphans(&expected.iter().map(Into::into).collect::<Vec<_>>())
            .map(|_| ())
    }
}

// The old implementation alone expands a terminal operation into its marker.
fn terminal_barrier(publication: &super::FilePublication) -> CommittedBundle {
    CommittedBundle {
        kind: crate::CommittedBundleKind::CheckpointedThrough,
        entries: Vec::new(),
        highest_protected_ordinal: publication.protected,
        total_committed_ordinals: publication.total,
    }
}

impl From<&CommittedState> for super::StructuralState {
    fn from(state: &CommittedState) -> Self {
        Self {
            files: state.entries.clone(),
            protected: state.highest_protected_ordinal,
            total: state.total_committed_ordinals,
            pending: state
                .orphaned_bundles
                .iter()
                .filter_map(|b| super::FilePublication::try_from(b).ok())
                .collect(),
        }
    }
}
impl From<&super::StructuralState> for CommittedState {
    fn from(state: &super::StructuralState) -> Self {
        Self {
            entries: state.files.clone(),
            highest_protected_ordinal: state.protected,
            total_committed_ordinals: state.total,
            orphaned_bundles: state.pending.iter().map(Into::into).collect(),
        }
    }
}
