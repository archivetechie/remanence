//! Tape authority entry point. Production binds only the per-tape log backend.
//! Callers retain these handles across media I/O; snapshots own frozen prefixes.
#[cfg(test)]
pub(crate) mod contract_tests;
#[cfg(any(test, feature = "fixtures"))]
pub mod fixtures;
mod import_legacy;
#[allow(dead_code)] // Retained legacy codec for B4 removal and codec characterization tests.
pub(crate) mod legacy;
#[cfg(test)]
pub(crate) mod legacy_fixtures;
pub use crate::tape_log::authority::{TapeAuthority, TapeAuthorityLease};
pub use import_legacy::{publish_import_authority, ImportPublication};
pub use legacy::AuthorityAgreementError;
pub(crate) use legacy::{structural_path, structural_path_in};
pub use remanence_parity::authority::{
    AuthoritySummary, TapeAuthoritySnapshot, TapeAuthorityView, TapeAuthorityWriter,
};

mod terminal;
pub use terminal::TerminalIndexSource;

/// Streaming catalog payload events shared by old and new authority backends.
/// Member rows are separate so a new backend need not materialize an Object's catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogPayload {
    /// Complete Object and copy metadata before its member rows.
    Object(Box<crate::tape_log::payload::CommittedObjectProjection>),
    /// One member of the current Object, in source order.
    Member(crate::NativeObjectFileProjectionInput),
    /// End of the current Object's member stream.
    ObjectEnd,
}

/// Complete catalog payload referenced by a durable commit. New backends may
/// retain this by reference and stream its members through the frozen cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectPayload {
    /// Format-neutral Object and copy metadata.
    pub projection: crate::tape_log::payload::CommittedObjectProjection,
    /// Catalog members in source order.
    pub files: Vec<crate::NativeObjectFileProjectionInput>,
}
/// Neutral commit/report used by both authority contracts.
pub type AuthorityCommit =
    remanence_parity::authority::CommitPublication<ObjectPayload, AuthorityFinalization>;

mod finalization;
pub use finalization::{
    AuthorityFinalization, AuthorityPrefixPlan, FinalizationRecovery,
    TerminalFinalizationComponent, TerminalFinalizationComponentKind, TerminalFinalizationLayout,
    TerminalFinalizationProgress,
};

impl std::ops::Deref for ObjectPayload {
    type Target = crate::tape_log::payload::CommittedObjectProjection;
    fn deref(&self) -> &Self::Target {
        &self.projection
    }
}
impl std::ops::DerefMut for ObjectPayload {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.projection
    }
}
/// Complete the media counters from a freshly assembled semantic commit.
pub fn complete_commit(mut commit: AuthorityCommit) -> AuthorityCommit {
    let files = commit
        .barrier
        .as_ref()
        .or_else(|| commit.object_files.last());
    commit.protected = files.map_or(0, |files| files.protected);
    commit.total = files.map_or_else(
        || {
            commit
                .payloads
                .last()
                .map_or(0, |payload| payload.total_committed_ordinals)
        },
        |files| files.total,
    );
    commit
}

pub(crate) mod validation;

mod bot_recovery;
pub use bot_recovery::BotRecoveryAuthority;
