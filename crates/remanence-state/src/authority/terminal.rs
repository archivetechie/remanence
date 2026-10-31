//! Backend-independent terminal source adapter over the shared authority view.
use super::{AuthorityFinalization, AuthorityPrefixPlan, TapeAuthorityView};
use crate::{StateError, TerminalIndexAuthorityReplayMetrics, TerminalIndexAuthoritySummary};
use remanence_parity::authority::{TerminalAuthoritySource, TerminalPrefixMode};
/// Frozen terminal reader selected solely through the authority contract.
#[derive(Debug)]
pub struct TerminalIndexSource(
    Box<
        dyn TerminalAuthoritySource<
            AuthorityFinalization,
            Error = StateError,
            ReplayMetrics = TerminalIndexAuthorityReplayMetrics,
        >,
    >,
);
impl TerminalIndexSource {
    /// Capture this terminal read mode through any conforming authority view.
    pub fn new_replay_backed<V>(
        view: &V,
        parity: &dyn remanence_parity::authority::StructuralAuthority,
    ) -> Result<Self, StateError>
    where
        V: TapeAuthorityView<
            Error = StateError,
            Finalization = AuthorityFinalization,
            TerminalPrefix = AuthorityPrefixPlan,
        >,
        V::TerminalSource: TerminalAuthoritySource<
            AuthorityFinalization,
            ReplayMetrics = TerminalIndexAuthorityReplayMetrics,
        >,
    {
        view.terminal_source(Some(parity), TerminalPrefixMode::Absent)
            .map(|source| Self(Box::new(source)))
    }
    /// Capture this terminal read mode through any conforming authority view.
    pub fn new_replay_backed_no_parity<V>(view: &V) -> Result<Self, StateError>
    where
        V: TapeAuthorityView<
            Error = StateError,
            Finalization = AuthorityFinalization,
            TerminalPrefix = AuthorityPrefixPlan,
        >,
        V::TerminalSource: TerminalAuthoritySource<
            AuthorityFinalization,
            ReplayMetrics = TerminalIndexAuthorityReplayMetrics,
        >,
    {
        view.terminal_source(None, TerminalPrefixMode::Absent)
            .map(|source| Self(Box::new(source)))
    }
    /// Capture this terminal read mode through any conforming authority view.
    pub fn new_replay_backed_after_terminal_prefix<V>(
        view: &V,
        parity: &dyn remanence_parity::authority::StructuralAuthority,
        prefix: &AuthorityPrefixPlan,
    ) -> Result<Self, StateError>
    where
        V: TapeAuthorityView<
            Error = StateError,
            Finalization = AuthorityFinalization,
            TerminalPrefix = AuthorityPrefixPlan,
        >,
        V::TerminalSource: TerminalAuthoritySource<
            AuthorityFinalization,
            ReplayMetrics = TerminalIndexAuthorityReplayMetrics,
        >,
    {
        view.terminal_source(Some(parity), TerminalPrefixMode::Durable(prefix))
            .map(|source| Self(Box::new(source)))
    }
    /// Capture this terminal read mode through any conforming authority view.
    pub fn new_replay_backed_with_planned_terminal_prefix<V>(
        view: &V,
        parity: &dyn remanence_parity::authority::StructuralAuthority,
        prefix: &AuthorityPrefixPlan,
    ) -> Result<Self, StateError>
    where
        V: TapeAuthorityView<
            Error = StateError,
            Finalization = AuthorityFinalization,
            TerminalPrefix = AuthorityPrefixPlan,
        >,
        V::TerminalSource: TerminalAuthoritySource<
            AuthorityFinalization,
            ReplayMetrics = TerminalIndexAuthorityReplayMetrics,
        >,
    {
        view.terminal_source(Some(parity), TerminalPrefixMode::Planned(prefix))
            .map(|source| Self(Box::new(source)))
    }
    /// Validated immutable row counts and scope.
    pub fn summary(&self) -> TerminalIndexAuthoritySummary {
        self.0.summary()
    }
    /// Bounded replay diagnostics captured by the backend.
    pub fn replay_metrics(&self) -> Option<TerminalIndexAuthorityReplayMetrics> {
        self.0.replay_metrics()
    }
    /// Reconstruct the canonical edition from the frozen source and neutral plan.
    pub fn reconstruct_final_edition(
        &mut self,
        intent: &AuthorityFinalization,
    ) -> Result<remanence_parity::TapeIndexEditionPlan, StateError> {
        Ok(self.0.reconstruct_final_plan(intent)?.edition)
    }
    /// Reconstruct and verify the canonical full terminal plan.
    pub fn reconstruct_final_plan(
        &mut self,
        intent: &AuthorityFinalization,
    ) -> Result<remanence_parity::TerminalTripleWritePlan, StateError> {
        self.0.reconstruct_final_plan(intent)
    }
}
impl remanence_parity::TapeIndexReplicaRecordSource for TerminalIndexSource {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(
            &remanence_parity::TapeIndexReplicaMapEntry,
        ) -> Result<(), remanence_parity::ParityError>,
    ) -> Result<(), remanence_parity::ParityError> {
        self.0.visit_structural_entries(visitor)
    }
    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(
            &remanence_parity::TapeIndexReplicaObjectRow,
        ) -> Result<(), remanence_parity::ParityError>,
    ) -> Result<(), remanence_parity::ParityError> {
        self.0.visit_object_rows(visitor)
    }
}
