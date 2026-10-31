//! Shared tape authority contracts. Parity owns structural evidence; state
//! supplies catalog payloads and intent storage without a reverse dependency.
//!
//! The production binding remains the two-journal implementation during B2.
//! Reports describe completed publication; they do not publish a second copy.

#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
mod identity;
mod publication;
pub use publication::{
    CommitPublication, FilePublication, FilePurpose, PublicationDisposition, PublicationReceipt,
    PublicationShapeError, PublishedItem, SidecarProvenance, StructuralState,
};
mod legacy;
pub use legacy::{TapeRowAuthority, TapeRowReader};
pub mod records;
mod rows;
pub use identity::{
    ManualTerminalFinalizationIdentity, SealDueReason, TerminalFinalizationTrigger,
};
pub use rows::{CommittedFileRows, FileRowCursor, FrozenFileRows};

use crate::{CommitCursor, JournalError};
#[cfg(any(test, feature = "fixtures"))]
use records::CommittedBundle;
use records::TapeFileEntry;

/// Small read model, independent of catalog member count and host encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthoritySummary {
    /// Physical tape identity.
    pub tape_uuid: [u8; 16],
    /// Durable checkpoint ordinal, including legacy sealed completion.
    pub checkpoint_ordinal: u64,
    /// Cumulative committed Object count.
    pub object_count: u64,
    /// Last durable media position; None denotes fresh authority.
    pub cursor: Option<CommitCursor>,
    /// Protected data ordinal W.
    pub protected: u64,
    /// Committed data ordinal T.
    pub total: u64,
    /// Next physical tape file number.
    pub next_file: u64,
    /// Fixed record size, once authority exists.
    pub block_size: Option<u32>,
    /// Admission obligation observed at the last barrier.
    pub seal_due: Option<SealDueReason>,
    /// Completed terminal authority.
    pub sealed: bool,
}

/// Fallible borrowed visitor used by bounded authority passes.
pub type AuthorityVisitor<'a, T, E> = dyn FnMut(&T) -> Result<(), E> + 'a;

/// A frozen authority supports independent structural, Object, and payload passes.
/// Visitors must propagate errors; they must never silently omit damaged data.
pub trait TapeAuthoritySnapshot {
    /// Storage and visitor error, preserving the owner's diagnostic type.
    type Error;
    /// One catalog event, allowing headers and member rows to stream separately.
    type Payload;
    /// Immutable finalization plan and progress at capture time.
    type Finalization;
    /// Cumulative counters and media position at this frozen boundary.
    fn summary(&self) -> &AuthoritySummary;
    /// Finalization facts captured at the same boundary as these rows.
    fn finalization(&self) -> Option<&Self::Finalization>;
    /// Transfer the frozen structural cursor for a parity media session.
    /// A parity-off snapshot has no separate structural cursor.
    fn into_file_rows(self) -> Option<FrozenFileRows>;
    /// Visit structural rows in physical order.
    fn visit_file_rows(
        &self,
        visitor: &mut dyn FnMut(&TapeFileEntry) -> Result<(), Self::Error>,
    ) -> Result<(), Self::Error>;
    /// Visit just Object identities, without decoding catalog members where supported.
    fn visit_object_rows(
        &self,
        visitor: &mut dyn FnMut(&crate::ObjectRecoveryRow) -> Result<(), Self::Error>,
    ) -> Result<(), Self::Error>;
    /// Visit complete Object catalog payloads in commit order.
    fn visit_payloads(
        &self,
        visitor: &mut AuthorityVisitor<'_, Self::Payload, Self::Error>,
    ) -> Result<(), Self::Error>;
}

/// Scope of a frozen terminal source, independent of its host storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalSourceSummary {
    /// Exact structural prefix represented by the edition.
    pub scope: crate::TapeIndexReplicaScope,
    /// Independent structural and Object row counts.
    pub counts: crate::TapeIndexReplicaCounts,
}
/// How a terminal source relates to the immutable prefix plan.
pub enum TerminalPrefixMode<P> {
    /// Ordinary pre-terminal prefix; terminal rows must be absent.
    Absent,
    /// Overlay an immutable plan over its validated base.
    Planned(P),
    /// Require the exact already durable prefix.
    Durable(P),
}
/// A backend's validated, frozen source for terminal replicas.
pub trait TerminalAuthoritySource<F>:
    crate::TapeIndexReplicaRecordSource + std::fmt::Debug
{
    /// Storage/validation error.
    type Error;
    /// Bounded replay diagnostics.
    type ReplayMetrics;
    /// Immutable row scope and counts.
    fn summary(&self) -> TerminalSourceSummary;
    /// Measurements taken while capturing this source.
    fn replay_metrics(&self) -> Option<Self::ReplayMetrics>;
    /// Reconstruct the exact planned edition and check its persisted digests.
    fn reconstruct_final_plan(
        &mut self,
        plan: &F,
    ) -> Result<crate::TerminalTripleWritePlan, Self::Error>;
}

/// Read-only tape authority. A freeze never follows later appends.
pub trait TapeAuthorityView {
    /// Typed storage error.
    type Error;
    /// Semantic commit projection used by compatibility consumers during B2.
    type Commit;
    /// Measurements of one bounded compatibility replay pass.
    type ReplayMetrics;
    /// Immutable intent and its current progress.
    type Finalization;
    /// Neutral prefix-plan input used when freezing terminal rows.
    type TerminalPrefix;
    /// Owned terminal source; it never borrows a mutable authority lease.
    type TerminalSource: TerminalAuthoritySource<Self::Finalization, Error = Self::Error> + 'static;
    /// Capture the caller's exact base/planned/durable terminal read mode.
    fn terminal_source(
        &self,
        structural: Option<&dyn StructuralAuthority>,
        prefix: TerminalPrefixMode<&Self::TerminalPrefix>,
    ) -> Result<Self::TerminalSource, Self::Error>;
    /// Last durable ordinary commit's admission fields. Catalog payloads and
    /// physical publication bundles may be omitted; use visit_commits or a frozen
    /// snapshot for complete payloads. Preserves the backend's admission rules.
    fn latest_commit(&self) -> Result<Option<Self::Commit>, Self::Error>;
    /// Stream semantic commits without retaining complete history.
    fn visit_commits(
        &self,
        visitor: &mut AuthorityVisitor<'_, Self::Commit, Self::Error>,
    ) -> Result<Self::ReplayMetrics, Self::Error>;
    /// Materialize semantic commits for existing offline consumers.
    /// Mutable access permits a backend cursor; this never publishes authority.
    fn replay_commits(&mut self) -> Result<Vec<Self::Commit>, Self::Error>;
    /// Pending intent only, preserving recovery routing after sealed completion.
    fn pending_finalization(&self) -> Result<Option<Self::Finalization>, Self::Error>;
    /// Owned read snapshot, independent of the writer borrow.
    type Snapshot: TapeAuthoritySnapshot<Error = Self::Error, Finalization = Self::Finalization>;
    /// Cumulative state at the last durable media boundary.
    fn authority_summary(&self) -> Result<AuthoritySummary, Self::Error>;
    /// Prove ordinary append agreement against the exact structural prefix.
    /// Terminal-prefix/component authority is not an ordinary append boundary.
    fn validate_append_rows(&self, rows: &FrozenFileRows) -> Result<(), Self::Error>;
    /// Capture one immutable prefix under the owner's lease. Snapshot agreement
    /// is the actor's storage-consistency check, not ordinary append admission:
    /// with a parity scheme, ordinary tapes require exact base-row agreement;
    /// sealed tapes and tapes with an intent use the recovery-aware terminal
    /// agreement (durable rows, identity, geometry and counters). Without a
    /// scheme no independent structural agreement is required. Implementations
    /// must never turn planned rows into durable rows. `validate_append_rows`
    /// remains the stricter ordinary-append proof regardless of this contract.
    fn freeze(&self) -> Result<Self::Snapshot, Self::Error>;
    /// Read finalization status, including a durably completed plan.
    fn finalization_status(&self) -> Result<Option<Self::Finalization>, Self::Error>;
}

/// Exclusive publication capability. Acquisition and retention of the lease
/// belong to the concrete compile-time binding, never to parity media code.
pub trait TapeAuthorityWriter: TapeAuthorityView {
    /// Publish one ordinary physical operation under this writer's lease.
    fn publish_commit(
        &mut self,
        publication: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError>;
    /// Publish the planned terminal prefix with its durability boundary.
    fn publish_terminal_prefix(
        &mut self,
        prefix: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError>;
    /// Publish one terminal component with its durability boundary.
    fn publish_terminal_component(
        &mut self,
        component: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError>;
    /// Complete semantic barrier publication including Object payloads.
    type Report;
    /// Existing progress enum; implementations enforce exact successors.
    type Progress;
    /// Validated recovery projection and pending plan.
    type Recovery;
    /// Reconcile a stale intent after sealed publication under this same lease.
    fn recover_finalization(&mut self) -> Result<Self::Recovery, Self::Error>;
    /// Publish a barrier report under the retained exclusive lease.
    fn publish_report(&mut self, report: &Self::Report) -> Result<(), Self::Error>;
    /// Publish an indivisible ordered group under the same lease.
    fn publish_reports(&mut self, reports: &[Self::Report]) -> Result<(), Self::Error>;
    /// Publish sealed completion, run the durable seam, then retire retry routing.
    fn publish_sealed_reports<F: FnOnce() -> Result<(), Self::Error>>(
        &mut self,
        reports: &[Self::Report],
        after_fsync: F,
    ) -> Result<(), Self::Error>;
    /// Identity protected by this exclusive lease.
    fn lease_tape_uuid(&self) -> [u8; 16];
    /// Publish the immutable plan before terminal media motion.
    fn begin_finalization(
        &mut self,
        plan: &Self::Finalization,
    ) -> Result<Self::Finalization, Self::Error>;
    /// Advance precisely one proved component.
    fn advance_finalization(
        &mut self,
        expected: Self::Progress,
        next: Self::Progress,
    ) -> Result<Self::Finalization, Self::Error>;
    /// Persist a recovery-required classification at the current progress.
    fn require_finalization_recovery(&mut self) -> Result<Self::Finalization, Self::Error>;
    /// Retire an unaccepted provisional manual plan with both catalog guards.
    fn retire_preparation(
        &mut self,
        expected: &Self::Finalization,
        catalog_present: bool,
        binding_present: bool,
    ) -> Result<(), Self::Error>;
}

/// Exact legacy publication observed at a media boundary. B2 keeps the old
/// append ordering, including the separate watermark, and reports its rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg(any(test, feature = "fixtures"))]
pub struct ShadowPublication {
    /// Units in publication order, including empty watermark units.
    pub bundles: Vec<CommittedBundle>,
}

#[cfg(any(test, feature = "fixtures"))]
impl ShadowPublication {
    /// A single successful legacy publication.
    pub fn one(bundle: CommittedBundle) -> Self {
        Self {
            bundles: vec![bundle],
        }
    }
    /// Check all semantic unit shapes without depending on their encoding.
    pub fn validate(&self) -> Result<(), JournalError> {
        for bundle in &self.bundles {
            records::validate_committed_bundle_shape(bundle)?;
        }
        Ok(())
    }
}

/// Media-facing publication facet. A report is one semantic operation; the old
/// implementation alone expands terminal operations into their two appends.
pub trait TapeReportPublisher {
    /// Tape identity of the retained publication capability.
    fn tape_uuid(&self) -> [u8; 16];
    /// Publish the reported units in their existing order. No second authority is written.
    #[cfg(any(test, feature = "fixtures"))]
    fn publish_fixture(&mut self, _report: &ShadowPublication) -> Result<(), JournalError> {
        Err(JournalError::Codec(
            "legacy fixture publication is not supported by this authority".into(),
        ))
    }

    /// Publish an ordinary physical operation; terminal kinds are refused here.
    fn publish_commit(
        &mut self,
        publication: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        refuse_terminal_commit(publication)?;
        Err(JournalError::Codec(
            "publication is not supported by this authority".into(),
        ))
    }
    /// Publish the rows and durable authority of the planned prefix.
    fn publish_terminal_prefix(
        &mut self,
        prefix: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        validate_terminal_publication(prefix, FilePurpose::TerminalPrefix)?;
        Err(JournalError::Codec(
            "publication is not supported by this authority".into(),
        ))
    }
    /// Publish the rows and durable authority of one terminal component.
    fn publish_terminal_component(
        &mut self,
        component: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        validate_terminal_publication(component, FilePurpose::TerminalComponent)?;
        Err(JournalError::Codec(
            "publication is not supported by this authority".into(),
        ))
    }
    /// Materialized legacy fixture evidence; production readers use frozen rows.
    #[cfg(any(test, feature = "fixtures"))]
    fn load_committed(&self) -> Result<records::CommittedState, JournalError> {
        Err(JournalError::Codec(
            "legacy fixture replay is not supported by this authority".into(),
        ))
    }
    /// Frozen rows for an in-process handoff; errors must never become absence.
    fn session_resume_snapshot(&self) -> Result<Option<FrozenFileRows>, JournalError> {
        self.committed_snapshot_bounded_authority().map(Some)
    }
    /// Frozen base at the exact planned terminal-prefix recovery boundary.
    fn session_terminal_resume_snapshot(
        &self,
        _prefix: &FilePublication,
    ) -> Result<Option<FrozenFileRows>, JournalError> {
        self.session_resume_snapshot()
    }
    /// Bounded structural authority; unsupported implementations fail closed.
    fn committed_snapshot_bounded_authority(&self) -> Result<FrozenFileRows, JournalError> {
        Err(JournalError::Codec(
            "journal does not provide bounded committed-prefix authority".into(),
        ))
    }
}

// Borrowing a facet never selects a different backing implementation.
impl<T: TapeReportPublisher + ?Sized> TapeReportPublisher for &mut T {
    fn tape_uuid(&self) -> [u8; 16] {
        (**self).tape_uuid()
    }
    #[cfg(any(test, feature = "fixtures"))]
    fn publish_fixture(&mut self, report: &ShadowPublication) -> Result<(), JournalError> {
        (**self).publish_fixture(report)
    }
    fn publish_commit(
        &mut self,
        publication: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        (**self).publish_commit(publication)
    }
    fn publish_terminal_prefix(
        &mut self,
        prefix: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        (**self).publish_terminal_prefix(prefix)
    }
    fn publish_terminal_component(
        &mut self,
        component: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        (**self).publish_terminal_component(component)
    }

    #[cfg(any(test, feature = "fixtures"))]
    fn load_committed(&self) -> Result<records::CommittedState, JournalError> {
        (**self).load_committed()
    }
    fn session_resume_snapshot(&self) -> Result<Option<FrozenFileRows>, JournalError> {
        (**self).session_resume_snapshot()
    }
    fn session_terminal_resume_snapshot(
        &self,
        prefix: &FilePublication,
    ) -> Result<Option<FrozenFileRows>, JournalError> {
        (**self).session_terminal_resume_snapshot(prefix)
    }
    fn committed_snapshot_bounded_authority(&self) -> Result<FrozenFileRows, JournalError> {
        (**self).committed_snapshot_bounded_authority()
    }
}

fn refuse_terminal_commit(publication: &FilePublication) -> Result<(), JournalError> {
    if matches!(
        publication.purpose,
        FilePurpose::TerminalPrefix | FilePurpose::TerminalComponent
    ) {
        return Err(JournalError::Codec(
            "terminal publication requires its explicit operation".into(),
        ));
    }
    Ok(())
}
fn validate_terminal_publication(
    unit: &FilePublication,
    expected: FilePurpose,
) -> Result<(), JournalError> {
    if unit.purpose != expected {
        return Err(JournalError::Codec(
            match expected {
                FilePurpose::TerminalPrefix => "invalid terminal-prefix/checkpoint transition",
                _ => "invalid terminal-component/checkpoint transition",
            }
            .into(),
        ));
    }
    Ok(())
}
fn terminal_receipt(
    publication: &FilePublication,
    disposition: [PublicationDisposition; 2],
) -> PublicationReceipt {
    PublicationReceipt {
        items: vec![PublishedItem {
            publication: publication.clone(),
            disposition: disposition[0],
            durability: disposition[1],
        }],
    }
}

/// Structural read/recovery facet borrowed from the selected tape writer.
/// Every operation names semantic media evidence, never a storage file.
pub trait StructuralAuthority: TapeReportPublisher + std::fmt::Debug + Send + 'static {
    /// Decode all stored fixture units, including pending operations.
    #[cfg(any(test, feature = "fixtures"))]
    fn fixture_publications(&self) -> Result<Vec<FilePublication>, JournalError>;
    /// Freeze the durable structural prefix.
    fn freeze(&self) -> Result<FrozenFileRows, JournalError>;
    /// Freeze the base preceding a planned prefix, including orphan validation.
    fn freeze_before_terminal_prefix(
        &self,
        prefix: &FilePublication,
    ) -> Result<FrozenFileRows, JournalError>;
    /// Freeze the already durable terminal prefix.
    fn freeze_terminal_prefix(&self) -> Result<FrozenFileRows, JournalError>;
    /// Whether the exact prefix and barrier are already durable.
    fn prefix_is_durable(&self, prefix: &FilePublication) -> Result<bool, JournalError>;
    /// Compare persisted component progress with the planned physical operations.
    fn component_relation(
        &self,
        completed: u8,
        expected: &[FilePublication],
    ) -> Result<crate::TerminalComponentAuthorityRelation, JournalError>;
    /// Count pending physical operations preserved when opening this capability.
    fn pending_count(&self) -> usize;
    /// Discard only the suffix already proved physically discarded by the owner.
    fn discard_suffix(&mut self, next_file: u64) -> Result<(), JournalError>;
    /// Discard the exact pending operations reconciled by the owner.
    fn discard_pending(&mut self, expected: &[FilePublication]) -> Result<(), JournalError>;
}
/// Owned structural capability; only the compile-time authority binding creates it.
pub type StructuralCapability = Box<dyn StructuralAuthority>;

impl<T: TapeReportPublisher + ?Sized> TapeReportPublisher for Box<T> {
    fn tape_uuid(&self) -> [u8; 16] {
        (**self).tape_uuid()
    }
    #[cfg(any(test, feature = "fixtures"))]
    fn publish_fixture(&mut self, report: &ShadowPublication) -> Result<(), JournalError> {
        (**self).publish_fixture(report)
    }
    fn publish_commit(
        &mut self,
        publication: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        (**self).publish_commit(publication)
    }
    fn publish_terminal_prefix(
        &mut self,
        prefix: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        (**self).publish_terminal_prefix(prefix)
    }
    fn publish_terminal_component(
        &mut self,
        component: &FilePublication,
    ) -> Result<PublicationReceipt, JournalError> {
        (**self).publish_terminal_component(component)
    }
    #[cfg(any(test, feature = "fixtures"))]
    fn load_committed(&self) -> Result<records::CommittedState, JournalError> {
        (**self).load_committed()
    }
    fn session_resume_snapshot(&self) -> Result<Option<FrozenFileRows>, JournalError> {
        (**self).session_resume_snapshot()
    }
    fn session_terminal_resume_snapshot(
        &self,
        prefix: &FilePublication,
    ) -> Result<Option<FrozenFileRows>, JournalError> {
        (**self).session_terminal_resume_snapshot(prefix)
    }
    fn committed_snapshot_bounded_authority(&self) -> Result<FrozenFileRows, JournalError> {
        (**self).committed_snapshot_bounded_authority()
    }
}
impl<T: StructuralAuthority + ?Sized> StructuralAuthority for Box<T> {
    #[cfg(any(test, feature = "fixtures"))]
    fn fixture_publications(&self) -> Result<Vec<FilePublication>, JournalError> {
        (**self).fixture_publications()
    }

    fn freeze(&self) -> Result<FrozenFileRows, JournalError> {
        (**self).freeze()
    }
    fn freeze_before_terminal_prefix(
        &self,
        prefix: &FilePublication,
    ) -> Result<FrozenFileRows, JournalError> {
        (**self).freeze_before_terminal_prefix(prefix)
    }
    fn freeze_terminal_prefix(&self) -> Result<FrozenFileRows, JournalError> {
        (**self).freeze_terminal_prefix()
    }
    fn prefix_is_durable(&self, prefix: &FilePublication) -> Result<bool, JournalError> {
        (**self).prefix_is_durable(prefix)
    }
    fn component_relation(
        &self,
        completed: u8,
        expected: &[FilePublication],
    ) -> Result<crate::TerminalComponentAuthorityRelation, JournalError> {
        (**self).component_relation(completed, expected)
    }
    fn pending_count(&self) -> usize {
        (**self).pending_count()
    }
    fn discard_suffix(&mut self, next_file: u64) -> Result<(), JournalError> {
        (**self).discard_suffix(next_file)
    }
    fn discard_pending(&mut self, expected: &[FilePublication]) -> Result<(), JournalError> {
        (**self).discard_pending(expected)
    }
}

#[cfg(test)]
mod default_publication_tests {
    use super::*;

    /// Enabling fixture support cannot opt a production publisher into append.
    #[test]
    fn default_publication_does_not_dispatch_to_fixture_hooks() {
        struct UnboundPublisher(usize);
        impl TapeReportPublisher for UnboundPublisher {
            fn tape_uuid(&self) -> [u8; 16] {
                [0; 16]
            }
            fn publish_fixture(&mut self, _: &ShadowPublication) -> Result<(), JournalError> {
                self.0 += 1;
                Ok(())
            }
        }
        let mut publisher = UnboundPublisher(0);
        let mut publication = FilePublication {
            purpose: FilePurpose::Checkpoint,
            files: Vec::new(),
            protected: 0,
            total: 0,
            cursor: None,
            check: None,
            seal_due: None,
        };
        assert!(publisher.publish_commit(&publication).is_err());
        publication.purpose = FilePurpose::TerminalPrefix;
        assert!(publisher.publish_terminal_prefix(&publication).is_err());
        publication.purpose = FilePurpose::TerminalComponent;
        assert!(publisher.publish_terminal_component(&publication).is_err());
        assert_eq!(publisher.0, 0);
    }
}
