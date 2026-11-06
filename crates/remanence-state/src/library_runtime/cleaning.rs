//! Cleaning owners publish their actual selected actions and source bindings.
//! The run head, cartridge registration and drive identity are authenticated
//! under dispatch; mutable projection rows never supply a predecessor.
use super::*;

impl LibraryRuntime {
    /// Register a physically observed barcode and slot. Reobservation preserves
    /// the original identity; the run's MovingIn records its selected home slot.
    pub fn register_cleaning_cartridge(
        &self,
        barcode: &str,
        home_slot: u16,
    ) -> Result<lib::TapeRef, RuntimeError> {
        self.register_cleaning_with_hook(barcode, home_slot, || Ok(()))
    }

    fn register_cleaning_with_hook(
        &self,
        barcode: &str,
        home_slot: u16,
        after_retire: impl FnOnce() -> Result<(), RuntimeError>,
    ) -> Result<lib::TapeRef, RuntimeError> {
        use crate::authority::TapeAuthorityView;
        let barcode: lib::LabelText = barcode.trim().into();
        let cartridge = lib::cleaning_cartridge_ref(&barcode);
        // The label lookup is source-backed. Retain the actual native admission
        // owner through the guarded retirement; no projector row proves empty.
        if let Some(uuid) = self.tape_for_label(barcode.as_str())? {
            let prior = self.tape_state(uuid).ok_or_else(|| {
                StateError::TapeProvisionConflict("cleaning data identity disappeared".into())
            })?;
            let reader = self.reader()?;
            let authority =
                crate::TapeAuthority::open(self.manifest().tape_root(), uuid, reader.path())?;
            let owner = authority.acquire_exclusive()?;
            let snapshot = owner.freeze()?;
            let sequence = snapshot.raw.summary().last_closed_sequence;
            let retirement = self.append_identity(&[&owner], |a, v, native| {
                let current = a.tape_state(uuid).ok_or({
                    AuthorityError::Store(StoreError::Fold(lib::FoldError::UnknownTape))
                })?;
                if current.retired
                    || current.label.as_ref() != Some(&barcode)
                    || current.tape != prior.tape
                {
                    return Err(AuthorityError::Store(StoreError::Fold(
                        lib::FoldError::ConflictingDuplicate,
                    )));
                }
                let instance_count = native.committed_instance_count(prior.tape);
                if instance_count != 0 {
                    return Err(AuthorityError::Store(
                        StoreError::TapeHoldsCommittedInstances {
                            tape_uuid: uuid::Uuid::from_bytes(uuid),
                            instance_count,
                        },
                    ));
                }
                a.retire(
                    lib::Retire {
                        tape: prior.tape,
                        reason: lib::RetireReason::OperatorRequested,
                        observed_tape_sequence: (sequence > 0).then_some(sequence),
                        assignment_generation: prior.assignment_generation,
                        detail: Some("inventory cleaning-cartridge reclassification".into()),
                    },
                    prior.assignment_generation,
                    native,
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    v,
                )
            });
            if matches!(
                &retirement,
                Err(RuntimeError::Authority(AuthorityError::Store(
                    StoreError::TapeHoldsCommittedInstances { .. }
                )))
            ) {
                let reason = format!(
                    "{{\"tape_uuid\":\"{}\",\"from\":\"data\",\"to\":\"cleaning\"}}",
                    uuid::Uuid::from_bytes(uuid).simple()
                );
                self.observe_alarm(
                    &format!(
                        "kind-flip-refused:{}",
                        uuid::Uuid::from_bytes(uuid).simple()
                    ),
                    Some(lib::AlarmRaise {
                        kind: lib::AlarmKind::KindFlipRefused,
                        severity: lib::Severity::Critical,
                        reason: reason.as_str().into(),
                    }),
                    "",
                )?;
            }
            retirement?;
        }
        after_retire()?;
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |_| Ok(()))
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "cleaning registration needs resolved authority".into(),
            )
            .into());
        }
        if replay
            .fold
            .cleaning_cartridges
            .contains_key(&cartridge.uuid)
        {
            self.catch_up_locked()?;
            return Ok(cartridge);
        }
        if replay.fold.tapes.values().any(|t| {
            !t.retired && (t.tape.uuid == cartridge.uuid || t.label.as_ref() == Some(&barcode))
        }) {
            return Err(StateError::TapeProvisionConflict(
                "cleaning registration requires guarded retirement of the existing data identity"
                    .into(),
            )
            .into());
        }
        let value = lib::CleaningRecorded {
            run_id: uuid::Uuid::new_v4().to_string().as_str().into(),
            previous: None,
            drive_uuid: None,
            cartridge: Some(cartridge),
            action: lib::CleaningAction::Register(lib::Register { barcode, home_slot }),
        };
        self.append_locked(|a, v| {
            a.cleaning_recorded(value, lib::AuditActor::System, lib::SourceLayer::Layer4, v)
        })?;
        Ok(cartridge)
    }

    /// Resolve the registered inventory identity from the source fold.
    pub fn cleaning_cartridge(&self, uuid: lib::Id) -> Result<lib::TapeRef, RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self.0.view.lock().unwrap_or_else(|e| e.into_inner());
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&*view, &mut |_| Ok(()))
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "cleaning lookup needs resolved authority".into(),
            )
            .into());
        }
        replay
            .fold
            .cleaning_cartridges
            .get(&uuid)
            .map(|c| lib::cleaning_cartridge_ref(&c.barcode))
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "cleaning cartridge has no registration source".into(),
                )
                .into()
            })
    }

    /// Publish a producer-selected run transition. Exact retries reuse the
    /// authenticated head, including a terminal result, without reopening it.
    pub fn record_cleaning_action(
        &self,
        run_id: &str,
        drive_uuid: Option<lib::Id>,
        cartridge: Option<lib::TapeRef>,
        action: lib::CleaningAction,
        actor: lib::AuditActor,
    ) -> Result<(), RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut head = None;
        let independent = matches!(
            action,
            lib::CleaningAction::Register(_)
                | lib::CleaningAction::Expire(_)
                | lib::CleaningAction::Reject(_)
        );
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::CleaningRecorded(prior) = applied.record().body_view() {
                    if prior.run_id.as_str() == run_id
                        && !matches!(
                            prior.action,
                            lib::CleaningAction::Register(_)
                                | lib::CleaningAction::Expire(_)
                                | lib::CleaningAction::Reject(_)
                        )
                    {
                        head = Some((
                            prior.clone(),
                            applied.record().reference(applied.sequence()),
                        ));
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "cleaning transition needs resolved authority".into(),
            )
            .into());
        }
        let mut value = lib::CleaningRecorded {
            run_id: run_id.into(),
            previous: None,
            drive_uuid,
            cartridge,
            action,
        };
        if !independent {
            if let Some((prior, reference)) = head {
                value.previous = prior.previous;
                if value == prior {
                    self.catch_up_locked()?;
                    return Ok(());
                }
                value.previous = Some(reference);
            }
        }
        self.append_locked(|a, v| a.cleaning_recorded(value, actor, lib::SourceLayer::Layer4, v))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::{TapeAuthorityView, TapeAuthorityWriter};

    /// Retire/Register survives a real injected source cut, native Initialized
    /// history at the same UUID, cache loss, and exact repeated registration.
    #[test]
    fn cleaning_reclassification_preserves_inventory_after_native_replay() {
        for same_uuid in [false, true] {
            for initialized in [0, 1, 2] {
                let root = tempfile::tempdir().unwrap();
                let root = root.path();
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
                let label: lib::LabelText = "CLN001L9".into();
                let canonical = lib::cleaning_cartridge_ref(&label);
                let id = if same_uuid {
                    canonical.uuid
                } else {
                    [0x49; 16]
                };
                runtime
                    .append(|a, v| {
                        a.reprovision(
                            lib::Reprovision {
                                tape: lib::TapeRef {
                                    uuid: id,
                                    incarnation: 1,
                                },
                                predecessor: None,
                                mode: lib::ProvisionMode::Initialize,
                                label: Some(label.clone()),
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
                if initialized > 0 {
                    let mut owner =
                        crate::TapeAuthority::open_fixture(paths.journal_dir.join("tapes"), id)
                            .unwrap()
                            .acquire_exclusive()
                            .unwrap();
                    if initialized == 2 {
                        owner
                            .publish_report(&(&crate::checkpoint::tests::record(id)).into())
                            .unwrap();
                        assert!(owner.freeze().unwrap().raw.summary().object_count > 0);
                    } else {
                        owner
                            .publish_initialized(
                                262144,
                                None,
                                remanence_parity::CommitCursor {
                                    partition: 0,
                                    lba: 2,
                                },
                            )
                            .unwrap();
                    }

                    runtime.refresh_native(&owner.freeze().unwrap()).unwrap();
                    runtime.catch_up().unwrap();
                }
                if initialized == 2 {
                    let error = runtime
                        .register_cleaning_cartridge(label.as_str(), 0x400)
                        .unwrap_err();
                    assert!(error.to_string().contains("committed"), "{error}");
                    assert!(!runtime.tape_state(id).unwrap().retired);
                    assert!(runtime.cleaning_cartridge(canonical.uuid).is_err());
                    let alarm = runtime
                        .reader()
                        .unwrap()
                        .get_alarm(&format!(
                            "kind-flip-refused:{}",
                            uuid::Uuid::from_bytes(id).simple()
                        ))
                        .unwrap()
                        .unwrap();
                    assert_eq!(alarm.state, "open");
                    assert_eq!(alarm.kind, "kind-flip-refused");
                    continue;
                }
                let cut = runtime
                    .register_cleaning_with_hook(label.as_str(), 0x400, || {
                        Err(
                            StateError::JournalReplayFailed("injected after retirement".into())
                                .into(),
                        )
                    })
                    .unwrap_err();
                assert!(cut.to_string().contains("injected after retirement"));
                assert!(runtime.tape_state(id).unwrap().retired);
                assert!(runtime.cleaning_cartridge(canonical.uuid).is_err());
                drop(runtime);
                let runtime = LibraryRuntime::open(&paths, true, lease.clone()).unwrap();
                if same_uuid {
                    // Focused projector fixture: every obsolete terminal field
                    // on the retired row must disappear at fresh Register.
                    rusqlite::Connection::open(&paths.sqlite_path).unwrap().execute("update tapes set finalization_progress='after-replica-c',finalization_trigger='operator-close-out',finalization_operation_id='old-operation',finalization_edition_digest=X'01',finalization_layout_digest=X'02',completed_replicas=3,finalization_outcome='complete',unprotected_since='old',last_verified_at_utc='old',last_verification_state=1,last_verified_edition_digest=X'03' where tape_uuid=?1",[id.as_slice()]).unwrap();
                }
                assert_eq!(
                    runtime
                        .register_cleaning_cartridge(label.as_str(), 0x400)
                        .unwrap(),
                    canonical
                );
                assert_eq!(
                    runtime
                        .register_cleaning_cartridge(label.as_str(), 0x401)
                        .unwrap(),
                    canonical
                );
                let live = runtime
                    .reader()
                    .unwrap()
                    .get_tape(&canonical.uuid)
                    .unwrap()
                    .unwrap();
                assert!(live.terminal_finalization.is_none());
                assert!(live.last_verified_at_utc.is_none());
                assert!(live.last_verification_state.is_none());
                assert!(live.last_verified_edition_digest.is_none());
                runtime
                    .rebuild_projection(
                        &mut crate::projector::open(&paths.sqlite_path)
                            .unwrap()
                            .write_handle(),
                    )
                    .unwrap();
                let row = runtime
                    .reader()
                    .unwrap()
                    .get_tape(&canonical.uuid)
                    .unwrap()
                    .unwrap();
                assert_eq!(row.kind, "cleaning");
                assert_eq!(row.state, "ready");
                assert_eq!(row.block_size, None);
                assert_eq!(row.written_extent_lba, None);
                assert_eq!(row.last_committed_tape_file, None);
                assert!(row.terminal_finalization.is_none());
                assert_eq!(row.assignment_generation, 0);
                assert!(row.pool_id.is_none());
                assert!(runtime.tape_state(id).unwrap().retired);
                drop(runtime);
                let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
                assert_eq!(
                    runtime
                        .reader()
                        .unwrap()
                        .get_tape(&canonical.uuid)
                        .unwrap()
                        .unwrap()
                        .kind,
                    "cleaning"
                );
            }
        }
    }
}
