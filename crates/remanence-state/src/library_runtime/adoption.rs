//! Bootstrap adoption uses the authenticated Reprovision source and current
//! native/fold state for retries. SQL and legacy audit mirrors cannot authorize
//! a no-op, evolve an identity, or supply missing physical facts.
use super::*;

impl LibraryRuntime {
    /// Adopt once, or prove an unchanged identity-only retry. A fresh invocation
    /// UUID may reuse identical physical facts without appending another source.
    ///
    /// `rule_pool` marks a pool derived from the configuration's rules: the caller leaves
    /// `value.pool_source` empty and this method stamps it with the latest configuration's
    /// reference, read under the dispatch lock (see `configuration_reference` for why that is
    /// the reference the fold checks).
    pub(crate) fn adopt_identity(
        &self,
        mut value: lib::Reprovision,
        rule_pool: bool,
        fresh_request_matches: impl FnOnce(&lib::Reprovision) -> bool,
    ) -> Result<bool, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let conflict =
            |message: &str| RuntimeError::State(StateError::TapeProvisionConflict(message.into()));
        if rule_pool {
            let (reference, _) = self
                .0
                .authority
                .configuration_reference()
                .ok_or_else(|| conflict("rule pool requires an accepted configuration"))?;
            value.pool_source = Some(lib::PoolSource::Rule(reference));
        }
        let adoption = value
            .adoption
            .as_ref()
            .ok_or_else(|| conflict("adoption evidence missing"))?;
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut original = None;
        let mut operation_conflict = false;
        let mut fences = std::collections::BTreeSet::new();
        let mut sessions = std::collections::BTreeSet::new();
        let mut readiness = std::collections::BTreeSet::new();
        let mut cached = false;
        let replay = self.0.authority.snapshot()?.replay(&view, &mut |applied| {
            let record = applied.record();
            match record.body_view() {
                lib::BodyView::FenceRaised(v) if matches!(&v.scope, lib::FenceScope::Tape(tape) if tape.uuid == value.tape.uuid) => { fences.insert(applied.sequence()); }
                lib::BodyView::FenceReleased(v) => { fences.remove(&v.fence); }
                lib::BodyView::SessionObserved(v) if v.tape == Some(value.tape) => {
                    if v.event == lib::SessionEvent::Opened { sessions.insert(v.session_id); } else { sessions.remove(&v.session_id); }
                }
                lib::BodyView::ReadinessObserved(v) => {
                    readiness.remove(&v.operation);
                    if (v.tape == Some(value.tape) || value.label.is_some() && v.barcode == value.label)
                        && !matches!(v.state, lib::ReadinessState::Ready | lib::ReadinessState::Released) {
                        readiness.insert(v.operation);
                    }
                }
                lib::BodyView::WrapMapCached(v) if v.tape == value.tape => cached = matches!(v.action, lib::WrapAction::Cache(_)),
                _ => (),
            }
            match record.body_view() {
                lib::BodyView::Reprovision(prior) if prior.tape.uuid == value.tape.uuid => {
                    original = Some(prior.clone());
                }
                _ if record.envelope.operation_id == Some(adoption.operation) => operation_conflict = true,
                _ => (),
            }
            Ok(())
        }).map_err(AuthorityError::Store)?;
        if operation_conflict {
            return Err(conflict(
                "adoption operation is already bound to a different recorded action",
            ));
        }
        if !fences.is_empty() || !sessions.is_empty() || !readiness.is_empty() || cached {
            return Err(conflict("Bootstrap identity is owned by an active fence, session, readiness operation or wrap cache"));
        }
        if replay.refusal.is_some()
            || !replay.fold.pending.is_empty()
            || replay
                .fold
                .contradictions
                .get(&lib::fold::Scope::Tape(value.tape.uuid))
                .is_some_and(|v| v.contradicted())
        {
            return Err(conflict(
                "adoption requires a fully resolved typed authority prefix",
            ));
        }
        if let Some(native) = view
            .snapshots()
            .find(|s| s.header().tape_uuid == value.tape.uuid)
        {
            let summary = native.summary();
            // Initialized already establishes a BOT file and written extent.
            // Only an absent log or its empty immutable header is identity-only.
            if summary.last_closed_sequence != 0 {
                return Err(conflict(
                    "Bootstrap identity evolved after adoption in native authority",
                ));
            }
        }
        if let Some(original) = original {
            let Some(prior_adoption) = original.adoption.as_ref() else {
                return Err(conflict(
                    "Bootstrap identity already belongs to a different provisioning ceremony",
                ));
            };
            let same_operation = prior_adoption.operation == adoption.operation;
            // A rule reference names the configuration current when each append was made, not a
            // fact of the adoption: a retry after a configuration change keeps its source.
            let mut expected = value.clone();
            if matches!(
                (&original.pool_source, &expected.pool_source),
                (
                    Some(lib::PoolSource::Rule(_)),
                    Some(lib::PoolSource::Rule(_))
                )
            ) {
                expected.pool_source = original.pool_source.clone();
            }
            if same_operation && original != expected
                || !same_operation && !fresh_request_matches(&original)
            {
                return Err(conflict("Bootstrap adoption changed immutable facts"));
            }
            let current = replay
                .fold
                .tapes
                .get(&value.tape.uuid)
                .ok_or_else(|| conflict("adoption source has no current typed identity"))?;
            if current.retired
                || current.tape != value.tape
                || current.assignment_generation != 1
                || current.label_generation != 1
                || current.media_revision != 1
                || current.medium != value.medium
                || current.label != value.label
                || current.pool != value.pool
            {
                return Err(conflict(
                    "Bootstrap identity assignment or media generation evolved after adoption",
                ));
            }
            self.catch_up_locked()?;
            return Ok(false);
        }
        self.append_locked(|authority, view| {
            authority.reprovision(
                value,
                lib::AuditActor::local_user(),
                lib::SourceLayer::Layer4,
                view,
            )
        })?;
        Ok(true)
    }
}
