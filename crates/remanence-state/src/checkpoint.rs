//! Checkpoint terminology retained for semantic callers; all storage lives in tape_log.
#[cfg(any(test, feature = "fixtures"))]
pub use crate::authority::{
    CommitFixture, ObjectProjectionFixture, ObjectRecoveryFixture,
    ObjectRecoveryFixtureRepresentation, TerminalFinalizationIntent,
    TerminalFinalizationPrefixPlan, TerminalFinalizationSidecarDirectoryEntry,
    TerminalRecoveryFixture,
};
pub use crate::tape_log::{
    CheckpointBoundedReplayMetrics, TerminalIndexAuthorityReplayMetrics,
    TerminalIndexAuthoritySummary,
};
pub use remanence_parity::authority::{
    ManualTerminalFinalizationIdentity, SealDueReason, TerminalFinalizationTrigger,
};

#[cfg(test)]
pub(crate) mod tests;

pub use crate::authority::{
    TerminalFinalizationComponent, TerminalFinalizationComponentKind, TerminalFinalizationLayout,
    TerminalFinalizationProgress,
};
