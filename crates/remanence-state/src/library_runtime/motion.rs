//! Typed implementation of the physical library's motion callbacks. Intent
//! publication uses the shared runtime's dispatch order and sticky failure latch;
//! completion evidence is copied directly from the command that actually ran.
use super::{LibraryRuntime, RuntimeError};
use crate::{library_log as lib, StateError};
use remanence_library::{motion::*, outcome};

pub(super) fn endpoint(key: &remanence_library::DriveKey) -> lib::DriveKey {
    match key {
        remanence_library::DriveKey::Bay {
            library_serial,
            bay,
        } => lib::DriveKey::Bay(lib::Bay {
            library_serial: library_serial.as_str().into(),
            bay: *bay,
        }),
        remanence_library::DriveKey::Standalone { drive_serial } => {
            lib::DriveKey::Standalone(lib::Standalone {
                drive_serial: drive_serial.as_str().into(),
            })
        }
    }
}
/// Preserve the captured SCSI fields; no error-string interpretation or later
/// REQUEST SENSE substitutes for the original completion.
pub(crate) fn evidence(value: &outcome::CompletionEvidence) -> lib::CompletionEvidence {
    lib::CompletionEvidence {
        dispatch: match value.dispatch {
            outcome::Dispatch::Issued => lib::Dispatch::Issued,
            outcome::Dispatch::NotIssued => lib::Dispatch::NotIssued,
        },
        completion: match value.completion {
            outcome::Completion::Known => lib::Completion::Known,
            outcome::Completion::Unknown => lib::Completion::Unknown,
        },
        scsi_status: value.scsi_status,
        host_status: value.host_status,
        driver_status: value.driver_status,
        sense: value.sense.clone().map(lib::Blob),
        residual: value.residual,
    }
}
fn inventory_elements(observed: &remanence_library::Library) -> Vec<lib::Element> {
    let mut elements = Vec::new();
    for slot in &observed.slots {
        elements.push(lib::Element {
            address: slot.element_address,
            kind: lib::ElementKind::Storage,
            barcode: slot.cartridge.as_deref().map(Into::into),
            full: slot.full,
        });
    }
    for port in &observed.ie_ports {
        elements.push(lib::Element {
            address: port.element_address,
            kind: lib::ElementKind::ImportExport,
            barcode: port.cartridge.as_deref().map(Into::into),
            full: port.full,
        });
    }
    for bay in &observed.drive_bays {
        elements.push(lib::Element {
            address: bay.element_address,
            kind: lib::ElementKind::Drive,
            barcode: bay.loaded_tape.as_deref().map(Into::into),
            full: bay.loaded,
        });
    }
    elements
}

impl LibraryRuntime {
    /// Resolve exactly the observed subject, preserving every other endpoint.
    fn reconcile_motion_observation(
        &self,
        subject: lib::InventorySubject,
        elements: Vec<lib::Element>,
    ) -> Result<(), RuntimeError> {
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        if self.motion_fenced() {
            return Err(RuntimeError::MotionFenced);
        }
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut open = std::collections::BTreeMap::new();
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                match applied.record().body_view() {
                    lib::BodyView::MotionIntent(value) => {
                        open.insert(
                            applied.sequence(),
                            (
                                applied.record().reference(applied.sequence()),
                                value.motion.clone(),
                            ),
                        );
                    }
                    lib::BodyView::MotionOutcome(value)
                        if value.result != lib::MotionResult::Unknown =>
                    {
                        open.remove(&value.intent.sequence);
                    }
                    lib::BodyView::InventoryReconciled(value) => {
                        for reference in &value.resolves.0 {
                            open.remove(&reference.sequence);
                        }
                    }
                    _ => (),
                }
                Ok(())
            })
            .map_err(lib::AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(crate::StateError::JournalReplayFailed(
                "startup motion reconciliation requires resolved source".into(),
            )
            .into());
        }
        let resolves = open
            .into_values()
            .filter(|(_, motion)| match (&subject, motion) {
                (lib::InventorySubject::Library(serial), lib::Motion::Move(v)) => {
                    serial == &v.library_serial
                }
                (
                    lib::InventorySubject::Library(serial),
                    lib::Motion::Load(lib::Load {
                        endpoint: lib::DriveKey::Bay(v),
                        ..
                    }),
                )
                | (
                    lib::InventorySubject::Library(serial),
                    lib::Motion::Unload(lib::Unload {
                        endpoint: lib::DriveKey::Bay(v),
                    }),
                ) => serial == &v.library_serial,
                (
                    lib::InventorySubject::Standalone(endpoint),
                    lib::Motion::Load(lib::Load {
                        endpoint: lib::DriveKey::Standalone(v),
                        ..
                    }),
                )
                | (
                    lib::InventorySubject::Standalone(endpoint),
                    lib::Motion::Unload(lib::Unload {
                        endpoint: lib::DriveKey::Standalone(v),
                    }),
                ) => endpoint == v,
                _ => false,
            })
            .map(|(reference, _)| reference)
            .collect::<Vec<_>>();
        if resolves.is_empty() {
            return Ok(());
        }
        self.append_locked(|a, v| {
            a.inventory_reconciled(
                lib::InventoryReconciled {
                    observation_generation: a.next_inventory_generation(&subject)?,
                    subject,
                    resolves: lib::Bounded(resolves),
                    complete: true,
                    elements: lib::Bounded(elements),
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer4,
                v,
            )
        })?;
        Ok(())
    }
    fn motion_error(&self, error: RuntimeError) -> MotionAuthorityError {
        fn authority_kind(error: &lib::AuthorityError) -> MotionAuthorityFailure {
            match error {
                lib::AuthorityError::MotionReconciliationRequired => {
                    MotionAuthorityFailure::UnresolvedMotion
                }
                lib::AuthorityError::AppliedBeforeRefusal { error, .. } => authority_kind(error),
                lib::AuthorityError::Store(lib::store::StoreError::Fold(
                    lib::FoldError::TapeUnavailable(_),
                )) => MotionAuthorityFailure::EvidenceFailure,
                lib::AuthorityError::Store(lib::store::StoreError::Fold(_))
                | lib::AuthorityError::FinalizationInProgress(_)
                | lib::AuthorityError::TapeHoldsCommittedInstances { .. }
                | lib::AuthorityError::OperationIdentityRequired => {
                    MotionAuthorityFailure::SemanticRefusal
                }
                lib::AuthorityError::Store(_) => MotionAuthorityFailure::StoreFailure,
                _ => MotionAuthorityFailure::EvidenceFailure,
            }
        }
        let kind = match &error {
            RuntimeError::Authority(error) => authority_kind(error),
            RuntimeError::AuthorityAndProjection { authority, .. }
                if authority_kind(authority) == MotionAuthorityFailure::StoreFailure =>
            {
                MotionAuthorityFailure::StoreFailure
            }
            RuntimeError::Projection(_) | RuntimeError::AuthorityAndProjection { .. } => {
                MotionAuthorityFailure::ProjectionFailure
            }
            RuntimeError::MotionFenced => MotionAuthorityFailure::StoreFailure,
            RuntimeError::State(_) => MotionAuthorityFailure::EvidenceFailure,
        };
        if !matches!(
            kind,
            MotionAuthorityFailure::SemanticRefusal | MotionAuthorityFailure::UnresolvedMotion
        ) {
            self.0
                .motion_fenced
                .store(true, std::sync::atomic::Ordering::Release);
        }
        MotionAuthorityError {
            message: error.to_string(),
            kind,
        }
    }
}
impl MotionAuthority for LibraryRuntime {
    fn identity(&self) -> Option<remanence_library::motion::MotionAuthorityIdentity> {
        Some(remanence_library::motion::MotionAuthorityIdentity {
            library_uuid: self.manifest().library_uuid(),
            library_root: self.manifest().library_root().to_path_buf(),
            tape_root: self.manifest().tape_root().to_path_buf(),
        })
    }
    fn requires_reconciliation(&self, command: &MotionCommand) -> bool {
        let motion = match command {
            MotionCommand::Move {
                library_serial,
                source,
                destination,
            } => lib::Motion::Move(lib::Move {
                library_serial: library_serial.as_str().into(),
                source: *source,
                destination: *destination,
            }),
            MotionCommand::Load(key) => lib::Motion::Load(lib::Load {
                endpoint: endpoint(key),
                attempt: 1,
            }),
            MotionCommand::Unload(key) => lib::Motion::Unload(lib::Unload {
                endpoint: endpoint(key),
            }),
        };
        self.0.authority.motion_requires_reconciliation(&motion)
    }
    fn reconcile_inventory(
        &self,
        observed: &remanence_library::Library,
    ) -> Result<(), MotionAuthorityError> {
        self.reconcile_motion_observation(
            lib::InventorySubject::Library(observed.serial.as_str().into()),
            inventory_elements(observed),
        )
        .map_err(|e| self.motion_error(e))
    }
    fn reconcile_standalone(
        &self,
        key: &remanence_library::DriveKey,
    ) -> Result<(), MotionAuthorityError> {
        let remanence_library::DriveKey::Standalone { drive_serial } = key else {
            return Err(MotionAuthorityError {
                kind: MotionAuthorityFailure::EvidenceFailure,
                message: "startup standalone resolver received bay".into(),
            });
        };
        self.reconcile_motion_observation(
            lib::InventorySubject::Standalone(lib::Standalone {
                drive_serial: drive_serial.as_str().into(),
            }),
            Vec::new(),
        )
        .map_err(|e| self.motion_error(e))
    }

    fn ensure_motion_allowed(&self) -> Result<(), MotionAuthorityError> {
        LibraryRuntime::ensure_motion_allowed(self).map_err(|error| self.motion_error(error))
    }
    fn intent(
        &self,
        command: &MotionCommand,
        context: &MotionContext,
    ) -> Result<MotionTicket, MotionAuthorityError> {
        LibraryRuntime::ensure_motion_allowed(self).map_err(|error| self.motion_error(error))?;
        let receipt = self
            .append(|authority, view| {
                if context.load_confirmation == LoadConfirmation::Readiness
                    && matches!(command, MotionCommand::Load(_))
                {
                    let operation = context.parent_operation.ok_or({
                        lib::AuthorityError::Store(lib::store::StoreError::Fold(
                            lib::FoldError::OperationIdentity,
                        ))
                    })?;
                    if let MotionCommand::Load(key) = command {
                        let (owner, _) = authority.readiness_context(operation, &endpoint(key))?;
                        if owner.is_none() {
                            return Err(lib::store::StoreError::Fold(
                                lib::FoldError::WrongCounterpart,
                            )
                            .into());
                        }
                    }
                }
                let motion = match command {
                    MotionCommand::Load(key) => {
                        let endpoint = endpoint(key);
                        let attempt =
                            authority.next_load_attempt(context.parent_operation, &endpoint)?;
                        lib::Motion::Load(lib::Load { endpoint, attempt })
                    }
                    MotionCommand::Unload(key) => lib::Motion::Unload(lib::Unload {
                        endpoint: endpoint(key),
                    }),
                    MotionCommand::Move {
                        library_serial,
                        source,
                        destination,
                    } => lib::Motion::Move(lib::Move {
                        library_serial: library_serial.as_str().into(),
                        source: *source,
                        destination: *destination,
                    }),
                };
                if authority.motion_requires_reconciliation(&motion) {
                    return Err(lib::AuthorityError::MotionReconciliationRequired);
                }
                authority.motion_intent(
                    lib::MotionIntent {
                        command: *uuid::Uuid::new_v4().as_bytes(),
                        parent_operation: context.parent_operation,
                        motion,
                        tape: context
                            .tape
                            .map(|(uuid, incarnation)| lib::TapeRef { uuid, incarnation }),
                        inventory_generation: context.inventory_generation,
                        readiness_generation: context.readiness_generation,
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer2,
                    view,
                )
            })
            .map_err(|error| self.motion_error(error))?;
        Ok(MotionTicket {
            sequence: receipt.sequence,
            record_uuid: receipt.record_uuid,
        })
    }
    fn unconfirmed(&self, pending: &PendingMotion) -> Result<(), MotionAuthorityError> {
        self.append(|authority, view| {
            authority.motion_outcome(
                lib::MotionOutcome {
                    intent: lib::DecisionRef {
                        sequence: pending.ticket.sequence,
                        record_uuid: pending.ticket.record_uuid,
                    },
                    result: lib::MotionResult::Unknown,
                    evidence: evidence(&pending.evidence),
                    confirmation: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer2,
                view,
            )
        })
        .map(|_| ())
        .map_err(|error| self.motion_error(error))
    }
    fn confirm_inventory(
        &self,
        pending: &PendingMotion,
        observed: &remanence_library::Library,
    ) -> Result<(), MotionAuthorityError> {
        let expected = match &pending.command {
            MotionCommand::Move { library_serial, .. } => Some(library_serial.as_str()),
            MotionCommand::Load(remanence_library::DriveKey::Bay { library_serial, .. })
            | MotionCommand::Unload(remanence_library::DriveKey::Bay { library_serial, .. }) => {
                Some(library_serial.as_str())
            }
            _ => None,
        };
        if expected != Some(observed.serial.as_str()) {
            return Err(MotionAuthorityError {
                message: "motion confirmation targets another changer".into(),
                kind: MotionAuthorityFailure::EvidenceFailure,
            });
        }
        let elements = inventory_elements(observed);
        let subject = lib::InventorySubject::Library(observed.serial.as_str().into());
        let inventory = self
            .append(|authority, view| {
                authority.inventory_reconciled(
                    lib::InventoryReconciled {
                        observation_generation: authority.next_inventory_generation(&subject)?,
                        subject,
                        resolves: lib::Bounded(Vec::new()),
                        complete: true,
                        elements: lib::Bounded(elements),
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer2,
                    view,
                )
            })
            .map_err(|error| self.motion_error(error))?;
        self.append(|authority, view| {
            authority.motion_outcome(
                lib::MotionOutcome {
                    intent: lib::DecisionRef {
                        sequence: pending.ticket.sequence,
                        record_uuid: pending.ticket.record_uuid,
                    },
                    result: lib::MotionResult::Done,
                    evidence: evidence(&pending.evidence),
                    confirmation: Some(lib::Confirmation::Inventory(inventory.sequence)),
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer2,
                view,
            )
        })
        .map(|_| ())
        .map_err(|error| self.motion_error(error))
    }
    fn confirm_standalone(&self, pending: &PendingMotion) -> Result<(), MotionAuthorityError> {
        let key = match &pending.command {
            MotionCommand::Load(remanence_library::DriveKey::Standalone { drive_serial })
            | MotionCommand::Unload(remanence_library::DriveKey::Standalone { drive_serial }) => {
                drive_serial
            }
            _ => {
                return Err(MotionAuthorityError {
                    message: "standalone observation targets a changer command".into(),
                    kind: MotionAuthorityFailure::EvidenceFailure,
                })
            }
        };
        let subject = lib::InventorySubject::Standalone(lib::Standalone {
            drive_serial: key.as_str().into(),
        });
        let inventory = self
            .append(|authority, view| {
                authority.inventory_reconciled(
                    lib::InventoryReconciled {
                        observation_generation: authority.next_inventory_generation(&subject)?,
                        subject,
                        resolves: lib::Bounded(Vec::new()),
                        complete: true,
                        elements: lib::Bounded(Vec::new()),
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer2,
                    view,
                )
            })
            .map_err(|error| self.motion_error(error))?;
        self.append(|authority, view| {
            authority.motion_outcome(
                lib::MotionOutcome {
                    intent: lib::DecisionRef {
                        sequence: pending.ticket.sequence,
                        record_uuid: pending.ticket.record_uuid,
                    },
                    result: lib::MotionResult::Done,
                    evidence: evidence(&pending.evidence),
                    confirmation: Some(lib::Confirmation::Inventory(inventory.sequence)),
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer2,
                view,
            )
        })
        .map(|_| ())
        .map_err(|error| self.motion_error(error))
    }
    fn confirm_readiness(
        &self,
        pending: &PendingMotion,
        observed: &remanence_library::MediaReadiness,
        _limit: std::time::Duration,
    ) -> Result<(), MotionAuthorityError> {
        if !observed.is_ready() {
            return Err(MotionAuthorityError {
                message: "readiness confirmation requires a ready probe".into(),
                kind: MotionAuthorityFailure::EvidenceFailure,
            });
        }
        let Some(operation) = pending.context.parent_operation else {
            return Err(MotionAuthorityError {
                message: "readiness-owned LOAD has no stable operation".into(),
                kind: MotionAuthorityFailure::EvidenceFailure,
            });
        };
        let MotionCommand::Load(key) = &pending.command else {
            return Err(MotionAuthorityError {
                message: "readiness cannot confirm MOVE or UNLOAD".into(),
                kind: MotionAuthorityFailure::EvidenceFailure,
            });
        };
        let endpoint = endpoint(key);
        let intent = lib::DecisionRef {
            sequence: pending.ticket.sequence,
            record_uuid: pending.ticket.record_uuid,
        };
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let (previous_source, owner) = self
            .readiness_source_locked(operation)
            .map_err(|e| self.motion_error(e))?
            .ok_or_else(|| {
                self.motion_error(
                    StateError::JournalReplayFailed("LOAD readiness owner is absent".into()).into(),
                )
            })?;
        if owner.endpoint != endpoint || owner.operation != operation {
            return Err(self.motion_error(
                StateError::JournalReplayFailed("LOAD readiness owner differs".into()).into(),
            ));
        }
        let observation_sequence = if owner.result == lib::ReadinessResult::Ready {
            // The wait callback publishes its actual TUR before invoking this
            // composer. Reuse that exact source, including its real phase.
            if previous_source.sequence <= intent.sequence
                || owner.state != lib::ReadinessState::Ready
                || owner.load_intent != Some(intent)
                || owner.evidence.as_ref() != Some(&evidence(&observed.outcome().evidence))
            {
                return Err(self.motion_error(
                    StateError::JournalReplayFailed(
                        "LOAD confirmation does not match its later ready observation".into(),
                    )
                    .into(),
                ));
            }
            previous_source.sequence
        } else {
            if owner.result != lib::ReadinessResult::Pending {
                return Err(self.motion_error(
                    StateError::JournalReplayFailed(
                        "LOAD readiness owner is terminal without ready proof".into(),
                    )
                    .into(),
                ));
            }
            self.append_locked(|authority, view| {
                let (_, ready_generation) = authority.readiness_context(operation, &endpoint)?;
                authority.readiness_observed(
                    lib::ReadinessObserved {
                        source_slot: None,
                        run: None,
                        cancel_source: None,
                        operation,
                        endpoint,
                        tape: owner.tape,
                        barcode: owner.barcode,
                        phase: lib::ReadinessPhase::Ready,
                        state: lib::ReadinessState::Ready,
                        previous: Some(previous_source),
                        started_unix_ns: owner.started_unix_ns,
                        limit_ms: owner.limit_ms,
                        evidence: Some(evidence(&observed.outcome().evidence)),
                        load_intent: Some(intent),
                        ready_generation,
                        result: lib::ReadinessResult::Ready,
                        scope: owner.scope,
                        media: owner.media,
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    view,
                )
            })
            .map_err(|error| self.motion_error(error))?
            .sequence
        };
        self.append_locked(|authority, view| {
            authority.motion_outcome(
                lib::MotionOutcome {
                    intent,
                    result: lib::MotionResult::Done,
                    evidence: evidence(&pending.evidence),
                    confirmation: Some(lib::Confirmation::Readiness(observation_sequence)),
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer2,
                view,
            )
        })
        .map(|_| ())
        .map_err(|error| self.motion_error(error))
    }

    fn failed(
        &self,
        ticket: MotionTicket,
        completion: &outcome::CompletionEvidence,
    ) -> Result<(), MotionAuthorityError> {
        let result = if completion.dispatch == outcome::Dispatch::Issued
            && completion.completion == outcome::Completion::Known
            && completion.failed
            && completion.scsi_status == Some(2)
        {
            lib::MotionResult::Failed
        } else {
            lib::MotionResult::Unknown
        };
        self.append(|authority, view| {
            authority.motion_outcome(
                lib::MotionOutcome {
                    intent: lib::DecisionRef {
                        sequence: ticket.sequence,
                        record_uuid: ticket.record_uuid,
                    },
                    result,
                    evidence: evidence(completion),
                    confirmation: None,
                },
                lib::AuditActor::System,
                lib::SourceLayer::Layer2,
                view,
            )
        })
        .map(|_| ())
        .map_err(|error| self.motion_error(error))
    }
}
