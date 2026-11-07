//! Manual terminal acceptance binds a real native Prepared frame. Reconciliation
//! reads authenticated library applications and never treats missing SQL as an
//! absent acceptance. The tape owner retains its admission lease throughout.
use super::*;

/// Authenticated acceptance retained in source, including after operation closure.
#[derive(Clone, Debug)]
pub struct ManualAcceptance {
    /// Current disposition, not merely the fact that bytes once existed.
    pub status: ManualAcceptanceStatus,
    /// Canonical original source identity.
    pub source: lib::DecisionRef,
    /// Exact immutable acceptance facts.
    pub value: lib::ManualFinalizationAccepted,
}
/// Existing source may answer a retry while forbidding new physical work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManualAcceptanceStatus {
    /// Native counterpart is proved and the fold applied this source.
    Applied,
    /// The fold is still waiting for a required source.
    Pending,
    /// Native contradiction/repair excluded the acceptance, or identity retired.
    Archived,
}
impl LibraryRuntime {
    /// Accept the real request once, including retries cut before native Prepared.
    /// Source scope lookup and writer minting share the dispatch lease.
    pub fn begin_manual_finalization(
        &self,
        accepted: lib::Accepted,
        actor: lib::AuditActor,
    ) -> Result<lib::Id, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut prior = None;
        let mut latest = None;
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::OperationRecorded(value) = applied.record().body_view() {
                    if let lib::OperationEvent::Accepted(original) = &value.event {
                        if value.kind == lib::OperationKind::FinalizeTape
                            && original.actor_fingerprint == accepted.actor_fingerprint
                            && original.idempotency_key == accepted.idempotency_key
                        {
                            if original != &accepted {
                                return Err(lib::FoldError::ConflictingDuplicate.into());
                            }
                            prior = Some(value.operation);
                        }
                    }
                    if prior == Some(value.operation) {
                        latest = Some(value.event.clone());
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "manual request needs resolved authority".into(),
            )
            .into());
        }
        if let Some(operation) = prior {
            if latest.as_ref().is_none_or(|event| !operation_open(event)) {
                self.record_refusal_locked(
                    lib::RefusalCode::ClosedOperationRetry,
                    RefusalSubject::Operation(operation),
                    actor,
                );
                return Err(StateError::TapeProvisionConflict(format!("ClosedOperationRetry: manual request operation is closed ({latest:?}); same-operation physical retry has no permitted transition")).into());
            }
            self.catch_up_locked()?;
            return Ok(operation);
        }
        let mut operation = None;
        self.append_locked(|a, v| {
            let (id, receipt) = a.begin_operation(
                lib::OperationKind::FinalizeTape,
                accepted,
                actor,
                lib::SourceLayer::Layer5,
                v,
            )?;
            operation = Some(id);
            Ok(receipt)
        })?;
        operation.ok_or_else(|| {
            StateError::JournalReplayFailed("manual writer did not mint operation".into()).into()
        })
    }

    /// Closed failure/unknown operations cannot authorize a physical resume:
    /// OperationRecorded forbids a later Finished under the same identity.
    pub fn ensure_manual_operation_continuable(
        &self,
        operation: lib::Id,
        actor: lib::AuditActor,
    ) -> Result<(), RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut latest = None;
        let mut accepted_tape = None;
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |record| {
                if let lib::BodyView::OperationRecorded(value) = record.record().body_view() {
                    if value.operation == operation {
                        latest = Some(value.event.clone());
                    }
                }
                if let lib::BodyView::ManualFinalizationAccepted(value) =
                    record.record().body_view()
                {
                    if value.operation == operation {
                        accepted_tape = Some(value.at.tape);
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "manual retry requires resolved operation source".into(),
            )
            .into());
        }
        let sealed = accepted_tape.is_some_and(|tape| {
            view.snapshots()
                .any(|s| s.header().tape_uuid == tape.uuid && s.summary().sealed)
        });
        if !latest.as_ref().is_some_and(|v| {
            operation_open(v) || (matches!(v, lib::OperationEvent::Finished(_)) && sealed)
        }) {
            self.record_refusal_locked(
                lib::RefusalCode::ClosedOperationRetry,
                RefusalSubject::Operation(operation),
                actor,
            );
            return Err(StateError::TapeProvisionConflict(format!("ClosedOperationRetry: manual operation cannot resume from {latest:?}; no permitted same-operation completion transition")).into());
        }
        Ok(())
    }

    /// Find a real Prepared closer for the owner operation, without opening the
    /// path under a second tape lock or naming a later Decided frame instead.
    pub fn manual_prepared_reference(
        &self,
        snapshot: &crate::tape_log::authority::AuthoritySnapshot,
        operation: lib::Id,
    ) -> Result<lib::TapeAt, RuntimeError> {
        let mut sequence = None;
        snapshot
            .raw
            .visit_frames(|frame| {
                if let crate::tape_log::Record::FinalizationPrepared(plan) = &frame.record {
                    if plan
                        .manual
                        .as_ref()
                        .is_some_and(|v| v.operation_id == operation)
                    {
                        sequence = Some(frame.sequence);
                    }
                }
                Ok(())
            })
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        let sequence = sequence.ok_or_else(|| {
            StateError::JournalReplayFailed(
                "manual finalization has no native Prepared source".into(),
            )
        })?;
        let evidence = snapshot
            .raw
            .record_evidence(sequence)
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?
            .ok_or_else(|| {
                StateError::JournalReplayFailed("Prepared source evidence disappeared".into())
            })?;
        Ok(lib::TapeAt {
            tape: lib::TapeRef {
                uuid: snapshot.raw.header().tape_uuid,
                incarnation: snapshot.log_reference().1,
            },
            sequence,
            record_digest: evidence.digest,
        })
    }

    /// Exact source lookup also answers an identical archived retry. A pending
    /// or refused replay cannot prove absence and must keep Prepared fenced.
    pub fn manual_acceptance(
        &self,
        operation: lib::Id,
    ) -> Result<Option<ManualAcceptance>, RuntimeError> {
        self.find_manual_acceptance(|value| value.operation == operation)
    }

    /// Retry lookup uses the original request scope even if SQL and the native
    /// Prepared have disappeared. The caller compares the returned fingerprint.
    pub fn manual_acceptance_by_request(
        &self,
        actor: &str,
        key: lib::Id,
    ) -> Result<Option<ManualAcceptance>, RuntimeError> {
        self.find_manual_acceptance(|value| {
            value.actor_fingerprint.as_str() == actor && value.idempotency_key == key
        })
    }

    fn find_manual_acceptance(
        &self,
        matches: impl Fn(&lib::ManualFinalizationAccepted) -> bool,
    ) -> Result<Option<ManualAcceptance>, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        self.find_manual_acceptance_locked(matches)
    }

    fn find_manual_acceptance_locked(
        &self,
        matches: impl Fn(&lib::ManualFinalizationAccepted) -> bool,
    ) -> Result<Option<ManualAcceptance>, RuntimeError> {
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut found: Option<ManualAcceptance> = None;
        let mut applied_sequences = std::collections::BTreeSet::new();
        let replay = self
            .0
            .authority
            .snapshot()?
            .inspect(
                &view,
                &mut |applied| {
                    if let lib::BodyView::ManualFinalizationAccepted(value) =
                        applied.record().body_view()
                    {
                        if matches(value) {
                            applied_sequences.insert(applied.sequence());
                        }
                    }
                    Ok(())
                },
                &mut |record, receipt| {
                    if let lib::BodyView::ManualFinalizationAccepted(value) = record.body_view() {
                        if matches(value) {
                            if let Some(prior) = &found {
                                if prior.value != *value {
                                    return Err(lib::FoldError::ConflictingDuplicate.into());
                                }
                            } else {
                                found = Some(ManualAcceptance {
                                    status: ManualAcceptanceStatus::Archived,
                                    source: record.reference(receipt.sequence),
                                    value: value.clone(),
                                });
                            }
                        }
                    }
                    Ok(())
                },
            )
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() {
            return Err(StateError::JournalReplayFailed(
                "manual acceptance lookup requires a resolved library prefix".into(),
            )
            .into());
        }
        if let Some(found) = &mut found {
            let tape = found.value.at.tape;
            if replay
                .fold
                .pending
                .iter()
                .any(|v| v.sequence == found.source.sequence)
            {
                found.status = ManualAcceptanceStatus::Pending;
            } else if applied_sequences.contains(&found.source.sequence)
                && replay
                    .fold
                    .tapes
                    .get(&tape.uuid)
                    .is_some_and(|v| v.tape == tape && !v.retired)
                && !replay
                    .fold
                    .contradictions
                    .get(&lib::fold::Scope::Tape(tape.uuid))
                    .is_some_and(|v| v.contradicted())
            {
                found.status = ManualAcceptanceStatus::Applied;
            }
        } else if !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "pending library source cannot prove acceptance absent".into(),
            )
            .into());
        }
        Ok(found)
    }

    /// Return the authenticated acceptance and current disposition. The caller
    /// may publish native Decided only when the returned status is Applied.
    pub fn accept_manual_finalization(
        &self,
        snapshot: &crate::tape_log::authority::AuthoritySnapshot,
        value: lib::ManualFinalizationAccepted,
        actor: lib::AuditActor,
    ) -> Result<ManualAcceptance, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        self.0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refresh(snapshot.raw.clone())
            .map_err(StateError::from)?;
        if let Some(existing) = self.find_manual_acceptance_locked(|prior| {
            prior.operation == value.operation
                || (prior.actor_fingerprint == value.actor_fingerprint
                    && prior.idempotency_key == value.idempotency_key)
        })? {
            if existing.value != value {
                return Err(StateError::TapeProvisionConflict(
                    "manual acceptance retry changed immutable facts".into(),
                )
                .into());
            }
            if existing.status == ManualAcceptanceStatus::Applied {
                self.catch_up_locked()?;
            }
            return Ok(existing);
        }
        self.append_locked(|a, v| {
            a.manual_finalization_accepted(value.clone(), actor, lib::SourceLayer::Layer5, v)
        })?;
        self.find_manual_acceptance_locked(|prior| prior.operation == value.operation)?
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "manual acceptance remains unapplied; keep native Prepared unresolved".into(),
                )
                .into()
            })
    }
}

fn operation_open(event: &lib::OperationEvent) -> bool {
    matches!(
        event,
        lib::OperationEvent::Accepted(_)
            | lib::OperationEvent::Started(_)
            | lib::OperationEvent::CancelRequested(_)
            | lib::OperationEvent::CancellationRejected(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::{TapeAuthorityView, TapeAuthorityWriter};
    use std::{io::Write, path::Path};

    /// The fixture uses the same native owner and typed writer as production;
    /// only physical tape bytes and operation request values are supplied here.
    fn fixture(
        root: &Path,
    ) -> (
        StatePaths,
        Arc<StateLockGuard>,
        LibraryRuntime,
        crate::TapeAuthorityLease,
        lib::ManualFinalizationAccepted,
    ) {
        let paths = StatePaths {
            config_path: root.join("config"),
            state_dir: root.join("state"),
            audit_dir: root.join("library"),
            journal_dir: root.join("native"),
            sqlite_path: root.join("catalog.sqlite"),
            tape_cache_dir: root.join("cache"),
            calibration_dir: root.join("calibration"),
        };
        let lease = Arc::new(StateLockGuard::acquire(&paths.state_dir).unwrap());
        let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
        let (owner, value) = prepare(&paths, &runtime, [0x7a; 16]);
        (paths, lease, runtime, owner, value)
    }

    fn prepare(
        paths: &StatePaths,
        runtime: &LibraryRuntime,
        id: lib::Id,
    ) -> (crate::TapeAuthorityLease, lib::ManualFinalizationAccepted) {
        let tape = lib::TapeRef {
            uuid: id,
            incarnation: 1,
        };
        runtime
            .append(|a, v| {
                a.reprovision(
                    lib::Reprovision {
                        pool_source: None,
                        operation: None,
                        replaced_bot: None,
                        tape,
                        predecessor: None,
                        mode: lib::ProvisionMode::Initialize,
                        label: Some(format!("MANUAL-{:02x}-L9", id[0]).as_str().into()),
                        label_provenance: Some(lib::LabelProvenance::Operator),
                        parity_scheme: None,
                        medium: lib::MediumSpec {
                            family: lib::MediumFamily::Lto(lib::LtoGen::Lto9),
                            usable_logical_bytes: 18_000_000_000_000,
                            record_size: 262144,
                            rerecord: true,
                            lifecycle_ops: true,
                            source: lib::GenerationSource::Operator,
                        },
                        pool: None,
                        endpoint: None,
                        adoption: None,
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            })
            .unwrap();
        let mut operation = None;
        runtime
            .append(|a, v| {
                let (id, receipt) = a.start_operation(
                    lib::OperationKind::FinalizeTape,
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer5,
                    v,
                )?;
                operation = Some(id);
                Ok(receipt)
            })
            .unwrap();
        let owner =
            crate::TapeAuthority::open_fixture(paths.journal_dir.join("tapes"), id).unwrap();
        let mut owner = owner.acquire_exclusive().unwrap();
        owner
            .publish_report(&(&crate::checkpoint::tests::record(id)).into())
            .unwrap();
        let mut plan = crate::checkpoint::tests::finalization_intent(id);
        let manual = plan.manual.as_mut().unwrap();
        manual.operation_id = operation.unwrap();
        manual.assignment_generation = 1;
        manual.assigned_pool_id = None;
        manual.expected_pool_id = None;
        let actor = manual.actor_fingerprint.clone();
        let key = manual.idempotency_key;
        let hash = manual.request_fingerprint;
        owner.begin_finalization(&(&plan).into()).unwrap();
        let snapshot = owner.freeze().unwrap();
        let value = lib::ManualFinalizationAccepted {
            at: runtime
                .manual_prepared_reference(&snapshot, operation.unwrap())
                .unwrap(),
            operation: operation.unwrap(),
            actor_fingerprint: actor.as_str().into(),
            idempotency_key: key,
            request_fingerprint: hash,
            assignment_generation: 1,
        };
        (owner, value)
    }

    #[test]
    fn native_prepared_acceptance_precedes_decided_and_exact_retry_survives_sql_loss() {
        let root = tempfile::tempdir().unwrap();
        let (paths, lease, runtime, mut owner, value) = fixture(root.path());
        assert!(
            owner
                .freeze()
                .unwrap()
                .raw
                .summary()
                .finalization
                .as_ref()
                .unwrap()
                .prepared
        );
        let accepted = runtime
            .accept_manual_finalization(
                &owner.freeze().unwrap(),
                value.clone(),
                lib::AuditActor::System,
            )
            .unwrap();
        assert_eq!(accepted.status, ManualAcceptanceStatus::Applied);
        assert!(
            owner
                .freeze()
                .unwrap()
                .raw
                .summary()
                .finalization
                .as_ref()
                .unwrap()
                .prepared,
            "acceptance does not manufacture native Decided"
        );
        owner.accept_preparation().unwrap();
        runtime.refresh_native(&owner.freeze().unwrap()).unwrap();
        assert_eq!(
            runtime
                .manual_prepared_reference(&owner.freeze().unwrap(), value.operation)
                .unwrap(),
            value.at,
            "retry still names Prepared, never Decided"
        );
        let count = runtime.test_source_records().len();
        assert_eq!(
            runtime
                .accept_manual_finalization(
                    &owner.freeze().unwrap(),
                    value.clone(),
                    lib::AuditActor::System
                )
                .unwrap()
                .source,
            accepted.source
        );
        assert_eq!(runtime.test_source_records().len(), count);
        drop(owner);
        drop(runtime);
        for suffix in ["", "-wal", "-shm"] {
            let p = std::path::PathBuf::from(format!("{}{suffix}", paths.sqlite_path.display()));
            if p.exists() {
                std::fs::remove_file(p).unwrap();
            }
        }
        let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
        let retry = runtime
            .manual_acceptance_by_request(value.actor_fingerprint.as_str(), value.idempotency_key)
            .unwrap()
            .unwrap();
        assert_eq!(retry.source, accepted.source);
        assert_eq!(retry.status, ManualAcceptanceStatus::Applied);
    }

    #[test]
    fn excluded_acceptance_answers_request_without_native_prepared_or_sql() {
        let root = tempfile::tempdir().unwrap();
        let (paths, lease, runtime, owner, value) = fixture(root.path());
        let accepted = runtime
            .accept_manual_finalization(
                &owner.freeze().unwrap(),
                value.clone(),
                lib::AuditActor::System,
            )
            .unwrap();
        drop(owner);
        drop(runtime);
        for path in crate::TapeAuthority::list_entries(paths.journal_dir.join("tapes")).unwrap() {
            std::fs::remove_file(path).unwrap();
        }
        for suffix in ["", "-wal", "-shm"] {
            let p = std::path::PathBuf::from(format!("{}{suffix}", paths.sqlite_path.display()));
            if p.exists() {
                std::fs::remove_file(p).unwrap();
            }
        }
        let before = std::fs::read_dir(&paths.audit_dir)
            .unwrap()
            .map(|v| {
                let p = v.unwrap().path();
                (p.clone(), std::fs::read(p).unwrap())
            })
            .collect::<Vec<_>>();
        let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
        let retry = runtime
            .manual_acceptance_by_request(value.actor_fingerprint.as_str(), value.idempotency_key)
            .unwrap()
            .unwrap();
        assert_eq!(retry.source, accepted.source);
        assert_eq!(retry.status, ManualAcceptanceStatus::Archived);
        for (path, bytes) in before {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
        assert!(
            crate::TapeAuthority::list_entries(paths.journal_dir.join("tapes"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn replay_duplicate_acceptance_preserves_original_applied_receipt() {
        let root = tempfile::tempdir().unwrap();
        let (paths, lease, runtime, owner, value) = fixture(root.path());
        let accepted = runtime
            .accept_manual_finalization(
                &owner.freeze().unwrap(),
                value.clone(),
                lib::AuditActor::System,
            )
            .unwrap();
        let mut original = runtime
            .test_source_records()
            .into_iter()
            .find(|r| matches!(r.body_view(), lib::BodyView::ManualFinalizationAccepted(_)))
            .unwrap();
        let uuid = runtime.manifest().library_uuid();
        drop(owner);
        drop(runtime);
        let view =
            NativeTapeView::probe(&paths.journal_dir.join("tapes"), &paths.sqlite_path).unwrap();
        let replay = lib::store::replay(&paths.audit_dir, uuid, &view).unwrap();
        original.envelope.record_uuid = *uuid::Uuid::new_v4().as_bytes();
        let (bytes, _) =
            lib::codec::encode(replay.last_sequence + 1, replay.terminal_hash, &original).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(paths.audit_dir.join(&replay.segments.last().unwrap().name))
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
        let retry = runtime
            .manual_acceptance_by_request(value.actor_fingerprint.as_str(), value.idempotency_key)
            .unwrap()
            .unwrap();
        assert_eq!(retry.source, accepted.source);
        assert_eq!(retry.status, ManualAcceptanceStatus::Applied);
    }
    #[test]
    fn concurrent_acceptance_scope_across_tapes_has_one_winner() {
        let root = tempfile::tempdir().unwrap();
        let (paths, _lease, runtime, owner_a, value_a) = fixture(root.path());
        let (owner_b, value_b) = prepare(&paths, &runtime, [0x7b; 16]);
        assert_eq!(value_a.idempotency_key, value_b.idempotency_key);
        let gate = Arc::new(std::sync::Barrier::new(3));
        let launch = |owner: crate::TapeAuthorityLease, value: lib::ManualFinalizationAccepted| {
            let runtime = runtime.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                let snapshot = owner.freeze().unwrap();
                gate.wait();
                runtime.accept_manual_finalization(&snapshot, value, lib::AuditActor::System)
            })
        };
        let a = launch(owner_a, value_a);
        let b = launch(owner_b, value_b);
        gate.wait();
        let results = [a.join().unwrap(), b.join().unwrap()];
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            runtime
                .test_source_records()
                .iter()
                .filter(|r| matches!(r.body_view(), lib::BodyView::ManualFinalizationAccepted(_)))
                .count(),
            1
        );
        for tape in [[0x7a; 16], [0x7b; 16]] {
            let owner =
                crate::TapeAuthority::open_fixture(paths.journal_dir.join("tapes"), tape).unwrap();
            assert!(
                owner
                    .freeze()
                    .unwrap()
                    .raw
                    .summary()
                    .finalization
                    .as_ref()
                    .unwrap()
                    .prepared,
                "neither request may skip the owner's acceptance check and publish Decided"
            );
        }
    }
}
