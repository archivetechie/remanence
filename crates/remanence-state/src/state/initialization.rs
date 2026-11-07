//! Initialization retains the actual predecessor's native admission owner across
//! the physical Bootstrap replacement. Successful destruction is then recorded
//! as Retire followed by Reprovision; unknown BOT identities are never invented.
use super::*;

/// Preflight proof and native ownership retained across a Bootstrap write.
pub struct InitializationGuard {
    previous: Option<crate::library_log::fold::TapeState>,
    owner: Option<crate::TapeAuthorityLease>,
    replaced_bot: Option<[u8; 16]>,
}
impl InitializationGuard {
    /// Foreign BOT identity checked by preflight, retained as overwrite evidence only.
    pub fn replaced_bot(&self) -> Option<[u8; 16]> {
        self.replaced_bot
    }
}
impl StateHandle {
    /// Validate known identity/label and exclude concurrent native finalization
    /// before destructive I/O. A missing predecessor source is a narrow refusal.
    pub fn prepare_tape_initialization(
        &self,
        previous: Option<[u8; 16]>,
        label: &str,
        clobbered_unknown: Option<[u8; 16]>,
    ) -> Result<InitializationGuard, StateError> {
        if clobbered_unknown.is_some() && previous != clobbered_unknown {
            return Err(StateError::TapeProvisionConflict(
                "named unknown BOT identity does not match the observed predecessor; no Bootstrap write admitted".into(),
            ));
        }
        self.library
            .ensure_motion_allowed()
            .map_err(|e| StateError::TapeProvisionConflict(e.to_string()))?;
        let previous = if let Some(unknown) = clobbered_unknown {
            if self.library.tape_state(unknown).is_some() {
                return Err(StateError::TapeProvisionConflict(
                    "named BOT identity is known; use the retire-and-replace path".into(),
                ));
            }
            None
        } else {
            previous.map(|id| self.library.tape_state(id).ok_or_else(||StateError::TapeProvisionConflict("observed BOT predecessor has no typed identity; no Bootstrap write admitted".into()))).transpose()?
        };
        let holder = self
            .library
            .tape_for_label(label)
            .map_err(|e| StateError::TapeProvisionConflict(e.to_string()))?;
        if holder.is_some_and(|id| previous.as_ref().is_none_or(|v| v.tape.uuid != id)) {
            return Err(StateError::TapeProvisionConflict(
                "initialization label is held by a different typed identity".into(),
            ));
        }
        let owner = previous
            .as_ref()
            .map(|prior| {
                let authority = crate::TapeAuthority::open(
                    self.paths.journal_dir.join("tapes"),
                    prior.tape.uuid,
                    &self.paths.sqlite_path,
                )?;
                let owner = authority.acquire_exclusive()?;
                let snapshot = owner.freeze()?;
                if let Some(reason) = snapshot
                    .tape_summary()
                    .closeout_required("initialization replacement")
                {
                    return Err(StateError::TapeProvisionConflict(reason));
                }
                Ok::<_, StateError>(owner)
            })
            .transpose()?;
        Ok(InitializationGuard {
            previous,
            owner,
            replaced_bot: clobbered_unknown,
        })
    }

    /// Called only after the physical owner reports a successful Bootstrap write.
    /// The preflight lease prevents old tape activity until its retirement lands.
    pub fn record_initialization_replacement(
        &mut self,
        guard: InitializationGuard,
    ) -> Result<Option<[u8; 16]>, StateError> {
        use crate::library_log as lib;
        let Some(prior) = guard.previous else {
            return Ok(None);
        };
        if !prior.retired {
            let owner = guard.owner.as_ref().ok_or_else(|| {
                StateError::TapeProvisionConflict("replacement lost its native owner".into())
            })?;
            let snapshot = owner.freeze()?;
            self.library
                .append_identity(&[owner], |a, v, native| {
                    a.retire(
                        lib::Retire {
                            tape: prior.tape,
                            reason: lib::RetireReason::OperatorRequested,
                            observed_tape_sequence: (snapshot.log_reference().0 != 0)
                                .then_some(snapshot.log_reference().0),
                            assignment_generation: prior.assignment_generation,
                            detail: Some(
                                "operator confirmed physical Bootstrap replacement".into(),
                            ),
                        },
                        prior.assignment_generation,
                        native,
                        AuditActor::local_user(),
                        SourceLayer::Layer4,
                        v,
                    )
                })
                .map_err(|e| StateError::TapeProvisionConflict(e.to_string()))?;
        }
        self.calibration.record_map_evicted(prior.tape.uuid)?;
        Ok(Some(prior.tape.uuid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::TapeAuthorityWriter as _;
    use crate::library_log as lib;
    fn state() -> (tempfile::TempDir, StateHandle) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(&path, super::super::tests::config_text(root.path())).unwrap();
        let state = StateHandle::open_from_config_file(path).unwrap();
        (root, state)
    }
    fn provision(state: &mut StateHandle, id: [u8; 16], previous: Option<[u8; 16]>) {
        provision_with_evidence(state, id, previous, None);
    }
    fn provision_with_evidence(
        state: &mut StateHandle,
        id: [u8; 16],
        previous: Option<[u8; 16]>,
        replaced_bot: Option<[u8; 16]>,
    ) {
        state
            .provision_replacing_media(
                crate::ProvisionTapeInput {
                    tape_uuid: id,
                    voltag: "ACM901L9".into(),
                    block_size: 262144,
                    parity: remanence_parity::ParityConfig::None,
                    force: previous.is_some(),
                },
                crate::LabelProvenance::Operator,
                Some(crate::MediaObservation::label_hint("ACM901L9").unwrap()),
                previous,
                replaced_bot,
            )
            .unwrap();
    }
    #[test]
    fn initialization_known_predecessor_retains_owner_until_real_replacement_record() {
        let (_root, mut state) = state();
        let old = [0x81; 16];
        let new = [0x82; 16];
        provision(&mut state, old, None);
        let guard = state
            .prepare_tape_initialization(Some(old), "ACM901L9", None)
            .unwrap();
        let native = crate::TapeAuthority::open(
            state.paths.journal_dir.join("tapes"),
            old,
            &state.paths.sqlite_path,
        )
        .unwrap();
        assert!(native.acquire_exclusive().is_err());
        assert!(
            !state.library.tape_state(old).unwrap().retired,
            "preflight must not retire before physical success"
        );
        let prior = state.record_initialization_replacement(guard).unwrap();
        provision(&mut state, new, prior);
        assert!(state.library.tape_state(old).unwrap().retired);
        assert_eq!(
            state
                .library
                .tape_state(new)
                .unwrap()
                .label
                .as_ref()
                .unwrap()
                .as_str(),
            "ACM901L9"
        );
        state.rebuild_index_from_journals().unwrap();
        assert_eq!(
            state.catalog().get_tape(&old).unwrap().unwrap().state,
            "retired"
        );
        assert_eq!(
            state
                .catalog()
                .get_tape_by_voltag("ACM901L9")
                .unwrap()
                .unwrap()
                .tape_uuid,
            new
        );
    }
    #[test]
    fn initialization_unknown_predecessor_is_refused_without_source_publication() {
        let (_root, state) = state();
        let before = state.library.test_source_records();
        assert!(state
            .prepare_tape_initialization(None, "ACM901L9", Some([0x83; 16]))
            .is_err());
        assert_eq!(state.library.test_source_records(), before);
    }
    #[test]
    fn ruling23_unknown_bot_is_evidence_only_after_initialization() {
        let (root, mut state) = state();
        let unknown = [0x91; 16];
        let fresh = [0x92; 16];
        let guard = state
            .prepare_tape_initialization(Some(unknown), "ACM901L9", Some(unknown))
            .unwrap();
        assert!(guard.previous.is_none());
        assert!(guard.owner.is_none());
        let evidence = guard.replaced_bot();
        assert_eq!(evidence, Some(unknown));
        let predecessor = state.record_initialization_replacement(guard).unwrap();
        assert_eq!(predecessor, None);
        provision_with_evidence(&mut state, fresh, predecessor, evidence);
        let verify = |state: &StateHandle| {
            let records = state.library.test_source_records();
            let provisions: Vec<_> = records
                .iter()
                .filter_map(|r| match r.body_view() {
                    lib::BodyView::Reprovision(v) => Some(v),
                    _ => None,
                })
                .collect();
            assert_eq!(provisions.len(), 1);
            let record = provisions[0];
            assert_eq!(record.tape.uuid, fresh);
            assert_eq!(record.mode, lib::ProvisionMode::Initialize);
            assert_eq!(record.predecessor, None);
            assert_eq!(record.replaced_bot, Some(unknown));
            assert_eq!(record.pool_source, None);
            assert_eq!(record.operation, None);
            assert!(records.iter().all(|r| r.tag() != lib::Tag::Retire));
            assert!(state.library.tape_state(unknown).is_none());
            assert!(state.catalog().get_tape(&unknown).unwrap().is_none());
            assert!(state.library.tape_state(fresh).is_some());
        };
        verify(&state);
        drop(state);
        let reopened = StateHandle::open_from_config_file(root.path().join("config.toml")).unwrap();
        verify(&reopened);
    }

    #[test]
    fn ruling23_unknown_bot_refusals_publish_no_records() {
        let (_root, mut state) = state();
        let unknown = [0x93; 16];
        let other = [0x94; 16];
        // A valid unknown identity is admitted; the refusals below are specific
        // protections, not the retired blanket C8-G4 refusal.
        let guard = state
            .prepare_tape_initialization(Some(unknown), "ACM901L9", Some(unknown))
            .unwrap();
        assert_eq!(guard.replaced_bot(), Some(unknown));
        drop(guard);
        for (previous, named) in [
            (None, Some(unknown)),
            (Some(other), Some(unknown)),
            (Some(unknown), None),
        ] {
            let before = state.library.test_source_records();
            assert!(state
                .prepare_tape_initialization(previous, "ACM901L9", named)
                .is_err());
            assert_eq!(state.library.test_source_records(), before);
        }
        provision(&mut state, other, None);
        for (previous, named, label, message) in [
            (Some(unknown), Some(unknown), "ACM901L9", "label is held"),
            (Some(other), Some(other), "FREE", "retire-and-replace"),
        ] {
            let before = state.library.test_source_records();
            let error = state
                .prepare_tape_initialization(previous, label, named)
                .err()
                .expect("protected identity must refuse");
            assert!(error.to_string().contains(message), "{error}");
            assert_eq!(state.library.test_source_records(), before);
        }
    }

    #[test]
    fn manual_request_cut_before_prepared_reuses_identity_but_startup_terminal_refuses() {
        let (_root, mut state) = state();
        let input = lib::Accepted {
            actor_fingerprint: "sha256:operator".into(),
            idempotency_key: Some([0x84; 16]),
            request_fingerprint: [0x85; 32],
            subject: Some("tape".into()),
        };
        let first = state
            .library
            .begin_manual_finalization(input.clone(), AuditActor::System)
            .unwrap();
        assert_eq!(
            state
                .library
                .begin_manual_finalization(input.clone(), AuditActor::System)
                .unwrap(),
            first
        );
        let count = state.library.test_source_records().len();
        state.startup_replay().unwrap();
        assert!(state
            .library
            .begin_manual_finalization(input, AuditActor::System)
            .unwrap_err()
            .to_string()
            .contains("closed"));
        assert_eq!(
            state.library.test_source_records().len(),
            count + 1,
            "only startup's actual Failed was added"
        );
        assert!(
            crate::TapeAuthority::list_entries(state.paths.journal_dir.join("tapes"))
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn identity_guards_use_native_owner_before_runtime_refresh_for_both_label_holders() {
        for prepared in [false, true] {
            for guard_prior in [false, true] {
                let (_root, mut state) = state();
                let prior = [0x91; 16];
                let target = [0x92; 16];
                provision(&mut state, prior, None);
                state
                    .provision_tape_with_media(
                        crate::ProvisionTapeInput {
                            tape_uuid: target,
                            voltag: "ACM902L9".into(),
                            block_size: 262144,
                            parity: remanence_parity::ParityConfig::None,
                            force: false,
                        },
                        crate::LabelProvenance::Operator,
                        crate::MediaObservation::label_hint("ACM902L9"),
                    )
                    .unwrap();
                let id = if guard_prior { prior } else { target };
                let native =
                    crate::TapeAuthority::open_fixture(state.paths.journal_dir.join("tapes"), id)
                        .unwrap();
                let mut owner = native.acquire_exclusive().unwrap();
                let mut report = crate::checkpoint::tests::record(id);
                if !prepared {
                    report.seal_due =
                        Some(crate::TerminalFinalizationTrigger::HardwareEarlyWarning);
                }
                owner.publish_report(&(&report).into()).unwrap();
                if prepared {
                    owner
                        .begin_finalization(
                            &(&crate::checkpoint::tests::finalization_intent(id)).into(),
                        )
                        .unwrap();
                }
                let before = state.library.test_source_records().len();
                assert!(state.assign_tape_pool(id, 1, None).is_err());
                assert!(state
                    .retire_tape(crate::RetireTapeInput {
                        tape_uuid: id,
                        reason: "guard test".into()
                    })
                    .is_err());
                assert!(state.rebind_tape_label(target, "ACM901L9", prior).is_err());
                assert_eq!(
                    state.library.test_source_records().len(),
                    before,
                    "held native owner prohibits identity source append before refresh"
                );
                drop(owner);
                assert!(state.assign_tape_pool(id, 1, None).is_err());
                assert!(state
                    .retire_tape(crate::RetireTapeInput {
                        tape_uuid: id,
                        reason: "guard test".into()
                    })
                    .is_err());
                assert!(state.rebind_tape_label(target, "ACM901L9", prior).is_err());
                assert_eq!(
                    state.library.test_source_records().len(),
                    before,
                    "fresh owner must discover Prepared or automatic seal debt"
                );
            }
        }
    }
}
