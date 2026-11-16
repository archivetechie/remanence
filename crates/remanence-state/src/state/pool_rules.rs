//! Accept startup configuration only after every proposed barcode pool move is safe.
//! Blank tapes move through the identity authority one lease at a time; uncertain
//! or written tapes refuse before configuration or assignment records are appended.

use super::{authority_reset, StateConfigWarning};
use crate::audit::{AuditActor, SourceLayer};
use crate::authority::TapeAuthorityView;
use crate::config::tape_pool_rule_for_voltag;
use crate::library_log as lib;
use crate::library_runtime::LibraryRuntime;
use crate::tape_log::fold::TapeSummary;
use crate::{RemConfig, StateError, StatePaths, TapeAuthority};

/// Whether startup accepts configuration and reapplies its barcode pool rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigurationPass {
    /// Evaluate all candidates, accept the configuration, then move blank tapes.
    Apply,
    /// Bypass evaluation and appends so an operator can pin a tape after refusal.
    ReportOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PoolRuleEvidence {
    Skip,
    Written,
    Unknown,
    Blank,
}

/// Keep terminal/recovery exclusions ahead of evidence that would refuse a move.
/// Any remaining tape log is uncertain, even with no committed objects.
fn classify_pool_rule_evidence(
    summary: Option<&TapeSummary>,
    dropped: bool,
    row_present: bool,
    terminal: bool,
    recovery: bool,
    copies: bool,
    session: bool,
) -> PoolRuleEvidence {
    if summary.is_some_and(|s| s.sealed || s.seal_due.is_some() || s.finalization.is_some())
        || terminal
        || recovery
    {
        PoolRuleEvidence::Skip
    } else if summary.is_some_and(|s| s.object_count > 0) {
        PoolRuleEvidence::Written
    } else if summary.is_some() || dropped || !row_present || copies || session {
        PoolRuleEvidence::Unknown
    } else {
        PoolRuleEvidence::Blank
    }
}

/// Accept the configuration and reapply rules to blank tapes before serving writes.
/// A conflict evaluates every candidate and appends nothing. After configuration
/// acceptance, an error returns immediately; the next start retries remaining moves.
pub fn accept_configuration(
    paths: &StatePaths,
    runtime: &LibraryRuntime,
    config: &RemConfig,
    pass: ConfigurationPass,
) -> Result<Vec<StateConfigWarning>, StateError> {
    if pass == ConfigurationPass::ReportOnly {
        return Ok(vec![]);
    }
    let reader = runtime.reader()?;
    let sessions = reader.non_terminal_sessions()?;
    let mut moves = Vec::new();
    let mut conflicts = Vec::new();
    for (tape, summary, dropped) in runtime.pool_rule_tapes() {
        let Some(label) = &tape.label else {
            continue;
        };
        if matches!(tape.pool_source, Some(lib::PoolSource::Operator(_))) {
            continue;
        }
        let rule = tape_pool_rule_for_voltag(label.as_str(), &config.tape_pool_rules);
        let derived = rule.map(|r| r.pool_id.as_str());
        let recorded = tape.pool.as_ref().map(|p| p.as_str());
        if derived == recorded {
            continue;
        }
        let uuid = tape.tape.uuid;
        let row = reader.get_tape(&uuid)?;
        let copies = !reader.committed_instance_pool_snapshots(&uuid)?.is_empty();
        let evidence = classify_pool_rule_evidence(
            summary.as_ref(),
            dropped,
            row.is_some(),
            row.as_ref()
                .is_some_and(|r| r.terminal_finalization.is_some()),
            row.as_ref().is_some_and(|r| r.state == "recovery_required"),
            copies,
            sessions
                .iter()
                .any(|s| s.tape_uuid.as_deref() == Some(uuid.as_slice())),
        );
        match evidence {
            PoolRuleEvidence::Skip => (),
            PoolRuleEvidence::Written | PoolRuleEvidence::Unknown => {
                let mut entry = format!(
                    "tape {} ({}) recorded {} derived {}",
                    uuid::Uuid::from_bytes(uuid),
                    label.as_str(),
                    recorded.unwrap_or("<unassigned>"),
                    derived.unwrap_or("<unassigned>"),
                );
                let unpinnable = if dropped {
                    Some("tape log unreadable")
                } else if row.is_none() {
                    Some("not catalogued")
                } else if recorded.is_some_and(|id| !config.tape_pools.iter().any(|p| p.id == id)) {
                    Some("recorded pool not configured")
                } else {
                    None
                };
                if let Some(reason) = unpinnable {
                    entry.push_str(", unpinnable: ");
                    entry.push_str(reason);
                }
                conflicts.push(entry);
            }
            PoolRuleEvidence::Blank => moves.push((
                tape.tape,
                tape.assignment_generation,
                label.as_str().to_owned(),
                recorded.map(str::to_owned),
                derived.map(str::to_owned),
                rule.map(|r| r.prefix.clone()),
            )),
        }
    }
    if !conflicts.is_empty() {
        return Err(StateError::TapePoolAssignmentConflict(format!(
            "the barcode pool rules would move tapes that may hold data: {}; restore the rule (and its pool, for \"recorded pool not configured\"), or pin each pinnable tape to its recorded pool with `rem tape assign-pool --tape UUID --pool RECORDED` (`--unassign` for <unassigned>) and start again",
            conflicts.join("; "),
        )));
    }
    let mut accepted = authority_reset::configuration_record(config)?;
    let prior_configuration = runtime.configuration();
    if let Some(prior) = &prior_configuration {
        accepted.generation = prior.generation;
    }
    if prior_configuration.as_ref() != Some(&accepted) {
        accepted.generation = prior_configuration.map_or(Ok(1), |v| {
            v.generation.checked_add(1).ok_or_else(|| {
                StateError::ConfigInvalid("configuration generation exhausted".into())
            })
        })?;
        runtime
            .append(|a, view| {
                a.configuration_accepted(accepted, AuditActor::System, SourceLayer::Layer4, view)
            })
            .map_err(|e| StateError::ConfigInvalid(e.to_string()))?;
    }
    let mut warnings = Vec::new();
    for (tape, expected, barcode, from, to, rule) in moves {
        let uuid = tape.uuid;
        let authority =
            TapeAuthority::open(paths.journal_dir.join("tapes"), uuid, &paths.sqlite_path)?;
        let owner = authority.acquire_exclusive()?;
        let frozen = owner.freeze()?;
        let current = frozen.tape_summary();
        if current.sealed
            || current.seal_due.is_some()
            || current.finalization.is_some()
            || current.object_count != 0
            || current.open_session.is_some()
            || current.last_closed_sequence != 0
            || current.incarnation != 0
        {
            return Err(StateError::TapePoolAssignmentConflict(format!(
                "tape {} changed before its pool rule move; start again",
                uuid::Uuid::from_bytes(uuid),
            )));
        }
        runtime
            .append_identity(&[&owner], |a, v, native| {
                let (reference, _) =
                    a.configuration_reference()
                        .ok_or(lib::AuthorityError::Store(lib::store::StoreError::Fold(
                            lib::FoldError::WrongCounterpart,
                        )))?;
                a.assign_pool(
                    lib::AssignPool {
                        tape,
                        pool: to.as_deref().map(Into::into),
                        change: lib::GenerationChange { expected, next: 0 },
                        source: lib::AssignmentSource::Rule,
                        configuration: Some(reference),
                    },
                    expected,
                    native,
                    AuditActor::System,
                    SourceLayer::Layer4,
                    v,
                )
            })
            .map_err(|e| StateError::TapePoolAssignmentConflict(e.to_string()))?;
        drop(owner);
        warnings.push(StateConfigWarning::TapePoolMoved {
            tape: uuid::Uuid::from_bytes(uuid),
            barcode,
            from,
            to,
            rule,
        });
    }
    Ok(warnings)
}

#[cfg(test)]
#[path = "pool_rules_tests.rs"]
mod tests;
