//! Readiness source publication from explicit operation inputs. No SQL row or
//! audit detail map supplies authority. An absent deadline is not silently
//! converted to a finite or unbounded typed duration.
use super::AuthorityError;
use super::{LibraryRuntime, RuntimeError};
use crate::{
    library_log as lib, MediaReadinessOperationInput, MediaReadinessTransitionInput, StateError,
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

fn invalid(detail: impl Into<String>) -> RuntimeError {
    StateError::JournalReplayFailed(detail.into()).into()
}

impl LibraryRuntime {
    /// Read the authoritative budget, including durable intents whose CDB has
    /// not yet completed. Projection counters cannot refund an intent.
    pub fn readiness_load_attempts(&self, operation: lib::Id) -> Result<u32, RuntimeError> {
        self.ensure_motion_allowed()?;
        self.0
            .authority
            .readiness_load_attempts(operation)
            .map_err(Into::into)
    }
    /// Publish the real creation facts before a readiness-owned LOAD. This
    /// converter deliberately refuses inputs the closed schema cannot express.
    pub fn begin_readiness(
        &self,
        input: &MediaReadinessOperationInput,
    ) -> Result<uuid::Uuid, RuntimeError> {
        self.begin_readiness_inner(input, false)
    }
    /// Preserve the caller's existing writer-minted operation identity across
    /// registry ownership, readiness observations and dependent LOAD intents.
    pub fn begin_owned_readiness(
        &self,
        input: &MediaReadinessOperationInput,
    ) -> Result<uuid::Uuid, RuntimeError> {
        self.begin_readiness_inner(input, true)
    }
    fn begin_readiness_inner(
        &self,
        input: &MediaReadinessOperationInput,
        owned: bool,
    ) -> Result<uuid::Uuid, RuntimeError> {
        // C8-G11: these actual durable presentation facts have no fields in
        // ReadinessObserved. Do not silently discard them or invent a side log.
        if input.run_id.is_some()
            || input.changer_sg.is_some()
            || input.drive_sg.is_some()
            || input.source_slot.is_some()
            || input.evidence_path.is_some()
        {
            return Err(invalid(
                "readiness owner metadata cannot be preserved by the available authority format",
            ));
        }
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let prior_source = self.readiness_source_locked(*input.operation_id.as_bytes())?;
        if let Some(serial) = input.drive_serial.as_deref() {
            let known = self
                .readiness_drive_serial_locked(&input.endpoint, *input.operation_id.as_bytes())?;
            if known.as_deref() != Some(serial) {
                return Err(invalid(
                    "readiness drive serial has no matching source identity",
                ));
            }
        }
        let deadline = input.deadline_at_utc.as_deref().ok_or_else(|| {
            invalid("readiness operation has no deadline; typed limit has no absent encoding")
        })?;
        let deadline = OffsetDateTime::parse(deadline, &Rfc3339)
            .map_err(|e| invalid(format!("invalid readiness deadline: {e}")))?;
        let now = OffsetDateTime::now_utc();
        let started_unix_ns =
            u64::try_from(now.unix_timestamp_nanos()).map_err(|e| invalid(e.to_string()))?;
        let deadline_ns =
            u64::try_from(deadline.unix_timestamp_nanos()).map_err(|e| invalid(e.to_string()))?;
        let endpoint = super::motion::endpoint(&input.endpoint);
        let phase = phase(&input.phase)?;
        let state = state(&input.state)?;
        if state == lib::ReadinessState::Ready {
            return Err(invalid(
                "ready observation requires an actual successful readiness probe",
            ));
        }
        let scope = match input.dirty_scope.as_deref() {
            Some("selected-library-snapshot") => lib::ReadinessScope::Library,
            Some("drive+tape") | Some("drive") | None => lib::ReadinessScope::Endpoint,
            Some(other) => return Err(invalid(format!("unsupported readiness scope {other:?}"))),
        };
        let media = input.media_family.map(lib::MediumFamily::Lto);
        if input.media_generation.is_some() && media.is_none() {
            return Err(invalid(
                "readiness producer must supply its exact media family",
            ));
        }
        if let (Some(number), Some(family)) = (input.media_generation, input.media_family) {
            if number != family.generation_number() {
                return Err(invalid(
                    "readiness media generation differs from its exact family",
                ));
            }
        }
        let result = match state {
            lib::ReadinessState::Ready => lib::ReadinessResult::Ready,
            lib::ReadinessState::Released => lib::ReadinessResult::Released,
            lib::ReadinessState::Cancelled => lib::ReadinessResult::Cancelled,
            lib::ReadinessState::AbortedUnknown => lib::ReadinessResult::AbortedUnknown,
            lib::ReadinessState::TerminalError => lib::ReadinessResult::Failed,
            // Nonterminal uncertainty remains an admission obligation.
            _ => lib::ReadinessResult::Pending,
        };
        if let Some((_, owner)) = &prior_source {
            let limit_ms = owner.limit_ms.ok_or_else(|| {
                invalid("readiness owner refresh changed immutable creation facts")
            })?;
            let represented_deadline = owner
                .started_unix_ns
                .checked_add(limit_ms.saturating_mul(1_000_000))
                .ok_or_else(|| invalid("readiness deadline overflow"))?;
            if owner.endpoint != endpoint
                || owner.barcode.as_ref().map(|b| b.as_str()) != input.barcode.as_deref()
                || owner.media != media
                || owner.scope != scope
                || deadline_ns.saturating_sub(represented_deadline) >= 1_000_000
                || deadline_ns < represented_deadline
            {
                return Err(invalid(
                    "readiness owner refresh changed immutable creation facts",
                ));
            }
            if owner.phase == phase && owner.state == state && owner.result == result {
                self.catch_up_locked()?;
                return Ok(input.operation_id);
            }
            if owner.result != lib::ReadinessResult::Pending {
                return Err(invalid("readiness operation is already terminal"));
            }
        }
        let mut operation = *input.operation_id.as_bytes();
        self.append_locked(|authority, view| {
            let (_, ready_generation) =
                authority.readiness_context(*input.operation_id.as_bytes(), &endpoint)?;
            let prior = prior_source;
            let (previous, started_unix_ns, limit_ms) = match prior {
                Some((source, owner)) => {
                    if owner.endpoint != endpoint {
                        return Err(
                            lib::store::StoreError::Fold(lib::FoldError::WrongCounterpart).into(),
                        );
                    }
                    (Some(source), owner.started_unix_ns, owner.limit_ms)
                }
                None => {
                    let duration = deadline_ns
                        .checked_sub(started_unix_ns)
                        .ok_or(lib::store::StoreError::Fold(lib::FoldError::InvalidSchema))?;
                    (None, started_unix_ns, Some(duration / 1_000_000))
                }
            };
            let value = lib::ReadinessObserved {
                source_slot: None,
                run: None,
                cancel_source: None,
                operation: *input.operation_id.as_bytes(),
                endpoint,
                tape: None,
                barcode: input.barcode.as_deref().map(Into::into),
                phase,
                state,
                previous,
                started_unix_ns,
                limit_ms,
                evidence: None,
                load_intent: None,
                ready_generation,
                result,
                scope,
                media,
            };
            if value.previous.is_some() {
                authority.readiness_observed(
                    value,
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    view,
                )
            } else if owned {
                authority.begin_readiness_for_operation(
                    value,
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    view,
                )
            } else {
                let (minted, receipt) = authority.begin_readiness(
                    value,
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer4,
                    view,
                )?;
                operation = minted;
                Ok(receipt)
            }
        })?;
        Ok(uuid::Uuid::from_bytes(operation))
    }
    /// Serial is redundant only when the endpoint or discovery source proves
    /// it at this owner's creation point. Later bay swaps cannot rewrite it.
    fn readiness_drive_serial_locked(
        &self,
        endpoint: &crate::DriveKey,
        operation: lib::Id,
    ) -> Result<Option<String>, RuntimeError> {
        if let crate::DriveKey::Standalone { drive_serial } = endpoint {
            return Ok(Some(drive_serial.clone()));
        }
        let endpoint = super::motion::endpoint(endpoint);
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut drives = std::collections::BTreeMap::<lib::Id, (lib::DriveObserved, bool)>::new();
        let mut endpoints = std::collections::BTreeMap::<lib::DriveKey, lib::Id>::new();
        let mut at_creation = None;
        let select =
            |drives: &std::collections::BTreeMap<lib::Id, (lib::DriveObserved, bool)>,
             endpoints: &std::collections::BTreeMap<lib::DriveKey, lib::Id>| {
                endpoints
                    .get(&endpoint)
                    .and_then(|id| drives.get(id))
                    .filter(|(_, retired)| !retired)
                    .and_then(|(drive, _)| drive.serial.as_ref())
                    .map(|serial| serial.as_str().to_owned())
            };
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                match applied.record().body_view() {
                    lib::BodyView::DriveObserved(drive) => {
                        let retired = drives
                            .get(&drive.drive_uuid)
                            .is_some_and(|(_, retired)| *retired);
                        drives.insert(drive.drive_uuid, (drive.clone(), retired));
                        // Match the source projector's endpoint ownership: the latest
                        // observation replaces a bay occupant, and a move vacates only
                        // entries still owned by this UUID.
                        endpoints.retain(|_, id| *id != drive.drive_uuid);
                        if let Some(endpoint) = &drive.endpoint {
                            endpoints.insert(endpoint.clone(), drive.drive_uuid);
                        }
                    }
                    lib::BodyView::DriveDecision(decision) => {
                        if let Some((_, retired)) = drives.get_mut(&decision.drive_uuid) {
                            match decision.decision {
                                lib::Stewardship::Retire(_) => *retired = true,
                                lib::Stewardship::Reinstate(_) => *retired = false,
                                lib::Stewardship::Annotate(_) => {}
                            }
                        }
                    }
                    lib::BodyView::ReadinessObserved(owner)
                        if owner.operation == operation && at_creation.is_none() =>
                    {
                        at_creation = Some(select(&drives, &endpoints));
                    }
                    _ => {}
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(invalid("readiness drive identity requires resolved source"));
        }
        Ok(at_creation.unwrap_or_else(|| select(&drives, &endpoints)))
    }

    /// Read the last authenticated source even when its terminal chain was
    /// evicted from the fold's bounded active index. SQL is never the predecessor.
    pub(super) fn readiness_source_locked(
        &self,
        operation: lib::Id,
    ) -> Result<Option<(lib::DecisionRef, lib::ReadinessObserved)>, RuntimeError> {
        if let Some(active) = self.0.authority.active_readiness(operation)? {
            return Ok(Some(active));
        }
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut latest = None;
        let replay = self
            .0
            .authority
            .snapshot()?
            .replay(&view, &mut |applied| {
                if let lib::BodyView::ReadinessObserved(value) = applied.record().body_view() {
                    if value.operation == operation {
                        latest = Some((
                            applied.record().reference(applied.sequence()),
                            value.clone(),
                        ));
                    }
                }
                Ok(())
            })
            .map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(invalid("readiness requires resolved authority"));
        }
        Ok(latest)
    }

    /// Resolve the exact outstanding LOAD from authenticated command history.
    /// A failed/Unknown outcome is not a successful pending command to confirm.
    fn pending_readiness_load_locked(
        &self,
        operation: lib::Id,
        endpoint: &lib::DriveKey,
    ) -> Result<Option<lib::DecisionRef>, RuntimeError> {
        let view = self
            .0
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut pending = std::collections::BTreeMap::new();
        let replay = self.0.authority.snapshot()?.replay(&view, &mut |applied| {
            match applied.record().body_view() {
                lib::BodyView::MotionIntent(value) if value.parent_operation == Some(operation)
                    && matches!(&value.motion, lib::Motion::Load(load) if &load.endpoint == endpoint) => {
                    pending.insert(applied.sequence(), applied.record().reference(applied.sequence()));
                }
                lib::BodyView::MotionOutcome(value) => { pending.remove(&value.intent.sequence); }
                lib::BodyView::InventoryReconciled(value) => {
                    for resolved in &value.resolves.0 { pending.remove(&resolved.sequence); }
                }
                _ => {},
            }
            Ok(())
        }).map_err(AuthorityError::Store)?;
        if replay.refusal.is_some() || !replay.fold.pending.is_empty() {
            return Err(invalid("readiness LOAD lookup requires resolved authority"));
        }
        if pending.len() > 1 {
            return Err(invalid("readiness has multiple unresolved LOAD commands"));
        }
        Ok(pending.into_values().next())
    }

    /// Advance explicit producer state using the immutable authenticated owner
    /// and actual captured completion. Missing completion is not inferred from
    /// a display opcode, decoded sense fields, or a diagnostic string.
    pub fn transition_readiness(
        &self,
        input: &MediaReadinessTransitionInput,
    ) -> Result<(), RuntimeError> {
        if input.cancel_source.is_some()
            || input.signal.is_some()
            || input.evidence_path.is_some()
            || input.last_error_json.is_some()
        {
            return Err(invalid("readiness transition metadata cannot be preserved by the available authority format"));
        }
        if input.completion_evidence.is_none()
            && (input.last_sense_raw.is_some()
                || input.last_sense_key.is_some()
                || input.last_asc.is_some()
                || input.last_ascq.is_some()
                || input.last_host_status.is_some()
                || input.last_driver_status.is_some()
                || input.target_status.is_some()
                || input.transport_class.is_some())
        {
            return Err(invalid(
                "readiness transition requires its actual command completion",
            ));
        }
        let _dispatch = self.0.dispatch.lock().unwrap_or_else(|e| e.into_inner());
        let (previous, mut value) = self
            .readiness_source_locked(*input.operation_id.as_bytes())?
            .ok_or_else(|| invalid("readiness transition has no authenticated owner"))?;
        let next_state = state(&input.state)?;
        let next_phase = input
            .phase
            .as_deref()
            .map(phase)
            .transpose()?
            .unwrap_or(value.phase);
        let next_result = readiness_result(next_state);
        if next_result == lib::ReadinessResult::Ready
            && (phase_opcode(next_phase) != Some(0)
                || !input
                    .completion_evidence
                    .as_ref()
                    .is_some_and(successful_probe))
        {
            return Err(invalid(
                "ready observation requires an actual successful readiness probe",
            ));
        }
        let next_scope = match input.dirty_scope.as_deref() {
            None => value.scope,
            Some("selected-library-snapshot") => lib::ReadinessScope::Library,
            Some("drive+tape" | "drive")
                if value.scope == lib::ReadinessScope::Endpoint
                    || next_result != lib::ReadinessResult::Pending =>
            {
                value.scope
            }
            Some("none") if next_result != lib::ReadinessResult::Pending => value.scope,
            _ => {
                return Err(invalid(
                    "readiness transition cannot narrow unresolved scope",
                ))
            }
        };
        if input
            .last_cdb_opcode
            .is_some_and(|opcode| Some(opcode) != phase_opcode(next_phase))
        {
            return Err(invalid(
                "readiness opcode cannot be derived from its source phase",
            ));
        }
        if input
            .quarantine_id
            .as_deref()
            .is_some_and(|id| id != format!("mrq-{}", input.operation_id))
        {
            return Err(invalid(
                "readiness quarantine identity is not the operation-derived identity",
            ));
        }
        if value.phase == next_phase
            && value.state == next_state
            && value.scope == next_scope
            && value.evidence == input.completion_evidence
        {
            self.catch_up_locked()?;
            return Ok(());
        }
        if value.result != lib::ReadinessResult::Pending {
            return Err(invalid("readiness operation is already terminal"));
        }
        value.scope = next_scope;
        if next_result == lib::ReadinessResult::Ready {
            value.load_intent =
                self.pending_readiness_load_locked(value.operation, &value.endpoint)?;
        }
        value.previous = Some(previous);
        value.phase = next_phase;
        value.state = next_state;
        value.result = next_result;
        value.evidence = input.completion_evidence.clone();
        value.ready_generation = self
            .0
            .authority
            .readiness_context(value.operation, &value.endpoint)?
            .1;
        self.append_locked(|a, v| {
            a.readiness_observed(value, lib::AuditActor::System, lib::SourceLayer::Layer4, v)
        })?;
        Ok(())
    }
}

/// Ready is produced only by an issued TUR that returned GOOD. The transport's
/// readiness classifier never turns CHECK CONDITION or unknown completion into
/// Ready, even when a display state supplied by a caller says otherwise.
fn successful_probe(evidence: &lib::CompletionEvidence) -> bool {
    evidence.dispatch == lib::Dispatch::Issued
        && evidence.completion == lib::Completion::Known
        && evidence.scsi_status == Some(0)
        && evidence.host_status.unwrap_or(0) == 0
        && evidence.driver_status.unwrap_or(0) == 0
        && evidence.sense.is_none()
        && evidence.residual.unwrap_or(0) == 0
}

/// These phase names explicitly denote a TUR observation; other command labels
/// need an authenticated command counterpart rather than a guessed opcode.
pub(crate) fn phase_opcode(phase: lib::ReadinessPhase) -> Option<u8> {
    match phase {
        lib::ReadinessPhase::ReadinessPoll
        | lib::ReadinessPhase::WaitReadyTur
        | lib::ReadinessPhase::StartupReconcileTur
        | lib::ReadinessPhase::SessionOpenShortProbe
        | lib::ReadinessPhase::SessionOpenAfterImmediateLoad
        | lib::ReadinessPhase::Ready => Some(0),
        _ => None,
    }
}

fn readiness_result(state: lib::ReadinessState) -> lib::ReadinessResult {
    match state {
        lib::ReadinessState::Ready => lib::ReadinessResult::Ready,
        lib::ReadinessState::Released => lib::ReadinessResult::Released,
        lib::ReadinessState::Cancelled => lib::ReadinessResult::Cancelled,
        lib::ReadinessState::AbortedUnknown => lib::ReadinessResult::AbortedUnknown,
        lib::ReadinessState::TerminalError => lib::ReadinessResult::Failed,
        _ => lib::ReadinessResult::Pending,
    }
}

fn phase(value: &str) -> Result<lib::ReadinessPhase, RuntimeError> {
    Ok(match value {
        "planned" => lib::ReadinessPhase::Planned,
        "initializing_command_load" => lib::ReadinessPhase::InitializingCommandLoad,
        "readiness_poll" => lib::ReadinessPhase::ReadinessPoll,
        "ready" => lib::ReadinessPhase::Ready,
        "cancelled" => lib::ReadinessPhase::Cancelled,
        "shutdown" => lib::ReadinessPhase::Shutdown,
        "session_open_short_probe" => lib::ReadinessPhase::SessionOpenShortProbe,
        "session_open_immediate_load" => lib::ReadinessPhase::SessionOpenImmediateLoad,
        "session_open_after_immediate_load" => lib::ReadinessPhase::SessionOpenAfterImmediateLoad,
        "startup_reconcile_tur" => lib::ReadinessPhase::StartupReconcileTur,
        "startup_reconcile" => lib::ReadinessPhase::StartupReconcile,
        "startup_reconcile_requires_release" => {
            lib::ReadinessPhase::StartupReconcileRequiresRelease
        }
        "quarantine_release" => lib::ReadinessPhase::QuarantineRelease,
        "released" => lib::ReadinessPhase::Released,
        "wait_ready_tur" => lib::ReadinessPhase::WaitReadyTur,
        "after_wait_ready_tur" => lib::ReadinessPhase::AfterWaitReadyTur,
        "before_conditional_immediate_load" => lib::ReadinessPhase::BeforeConditionalImmediateLoad,
        "conditional_immediate_load" => lib::ReadinessPhase::ConditionalImmediateLoad,
        "after_conditional_immediate_load" => lib::ReadinessPhase::AfterConditionalImmediateLoad,
        "after_bot_classification" => lib::ReadinessPhase::AfterBotClassification,
        "after_clobber_confirmation" => lib::ReadinessPhase::AfterClobberConfirmation,
        "after_move_medium" => lib::ReadinessPhase::AfterMoveMedium,
        "after_open_drive" => lib::ReadinessPhase::AfterOpenDrive,
        "after_open_library" => lib::ReadinessPhase::AfterOpenLibrary,
        "after_read_medium_partitions" => lib::ReadinessPhase::AfterReadMediumPartitions,
        "after_rewind_before_bot_read" => lib::ReadinessPhase::AfterRewindBeforeBotRead,
        "apply_init_write_gate" => lib::ReadinessPhase::ApplyInitWriteGate,
        "before_apply_init_write_gate" => lib::ReadinessPhase::BeforeApplyInitWriteGate,
        "before_move_medium" => lib::ReadinessPhase::BeforeMoveMedium,
        "before_open_drive" => lib::ReadinessPhase::BeforeOpenDrive,
        "before_open_library" => lib::ReadinessPhase::BeforeOpenLibrary,
        "before_read_drive_config" => lib::ReadinessPhase::BeforeReadDriveConfig,
        "before_rewind_before_bootstrap_write" => {
            lib::ReadinessPhase::BeforeRewindBeforeBootstrapWrite
        }
        "before_rewind_before_bot_read" => lib::ReadinessPhase::BeforeRewindBeforeBotRead,
        "before_wait_ready_tur" => lib::ReadinessPhase::BeforeWaitReadyTur,
        "move_medium" => lib::ReadinessPhase::MoveMedium,
        "open_drive" => lib::ReadinessPhase::OpenDrive,
        "open_library" => lib::ReadinessPhase::OpenLibrary,
        "read_drive_config" => lib::ReadinessPhase::ReadDriveConfig,
        "read_medium_partitions" => lib::ReadinessPhase::ReadMediumPartitions,
        "rewind_before_bootstrap_write" => lib::ReadinessPhase::RewindBeforeBootstrapWrite,
        "rewind_before_bot_read" => lib::ReadinessPhase::RewindBeforeBotRead,
        "clobber_confirmation" => lib::ReadinessPhase::ClobberConfirmation,
        "pre_load_tur" => lib::ReadinessPhase::PreLoadTur,
        "already_loaded_tur" => lib::ReadinessPhase::AlreadyLoadedTur,
        "before_pre_load_tur" => lib::ReadinessPhase::BeforePreLoadTur,
        "after_pre_load_tur" => lib::ReadinessPhase::AfterPreLoadTur,
        "before_already_loaded_tur" => lib::ReadinessPhase::BeforeAlreadyLoadedTur,
        "after_already_loaded_tur" => lib::ReadinessPhase::AfterAlreadyLoadedTur,
        _ => return Err(invalid(format!("unsupported ReadinessPhase {value:?}"))),
    })
}

fn state(value: &str) -> Result<lib::ReadinessState, RuntimeError> {
    Ok(match value {
        "planned" => lib::ReadinessState::Planned,
        "pre_ready_loading" => lib::ReadinessState::PreReadyLoading,
        "ready" => lib::ReadinessState::Ready,
        "media_initializing" => lib::ReadinessState::MediaInitializing,
        "becoming_ready" => lib::ReadinessState::BecomingReady,
        "unit_attention" => lib::ReadinessState::UnitAttention,
        "target_busy" => lib::ReadinessState::TargetBusy,
        "reservation_conflict" => lib::ReadinessState::ReservationConflict,
        "transport_unknown" => lib::ReadinessState::TransportUnknown,
        "terminal_error" => lib::ReadinessState::TerminalError,
        "timeout_unknown" => lib::ReadinessState::TimeoutUnknown,
        "aborted_unknown" => lib::ReadinessState::AbortedUnknown,
        "cancelled" => lib::ReadinessState::Cancelled,
        "released" => lib::ReadinessState::Released,
        "starting" => lib::ReadinessState::Starting,
        "recording_failed" => lib::ReadinessState::RecordingFailed,
        _ => return Err(invalid(format!("unsupported ReadinessState {value:?}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Forged/closed owner IDs never append, while a partial observation's
    /// whole-library uncertainty survives exact retries and projection loss.
    #[test]
    fn readiness_owner_identity_scope_retry_and_source_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let paths = crate::StatePaths {
            config_path: root.join("config"),
            state_dir: root.join("state"),
            audit_dir: root.join("library"),
            journal_dir: root.join("native"),
            sqlite_path: root.join("catalog.sqlite"),
            tape_cache_dir: root.join("cache"),
            calibration_dir: root.join("calibration"),
        };
        let lease = Arc::new(crate::StateLockGuard::acquire(&paths.state_dir).unwrap());
        let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
        let operation = runtime
            .start_owned_operation(
                lib::OperationKind::MediaReadinessWait,
                lib::AuditActor::System,
            )
            .unwrap();
        let input = MediaReadinessOperationInput {
            operation_id: uuid::Uuid::from_bytes(operation),
            run_id: None,
            endpoint: crate::DriveKey::new("LIB", 0x100),
            changer_sg: None,
            drive_sg: None,
            drive_serial: None,
            barcode: Some("READY1L9".into()),
            source_slot: None,
            media_generation: Some(9),
            media_family: Some(crate::LtoGen::Lto9),
            phase: "readiness_poll".into(),
            state: "planned".into(),
            dirty_scope: Some("drive+tape".into()),
            deadline_at_utc: Some(
                (OffsetDateTime::now_utc() + time::Duration::seconds(60))
                    .format(&Rfc3339)
                    .unwrap(),
            ),
            evidence_path: None,
        };
        let before = runtime.test_source_records().len();
        let mut forged = input.clone();
        forged.operation_id = uuid::Uuid::new_v4();
        assert!(runtime.begin_owned_readiness(&forged).is_err());
        assert_eq!(runtime.test_source_records().len(), before);
        assert_eq!(
            runtime.begin_owned_readiness(&input).unwrap(),
            input.operation_id
        );
        let owner_count = runtime.test_source_records().len();
        runtime.begin_owned_readiness(&input).unwrap();
        assert_eq!(runtime.test_source_records().len(), owner_count);
        let transition = MediaReadinessTransitionInput {
            completion_evidence: None,
            operation_id: input.operation_id,
            phase: None,
            state: "planned".into(),
            dirty_scope: Some("selected-library-snapshot".into()),
            last_cdb_opcode: None,
            last_sense_raw: None,
            last_sense_key: None,
            last_asc: None,
            last_ascq: None,
            last_host_status: None,
            last_driver_status: None,
            target_status: None,
            transport_class: None,
            cancel_source: None,
            signal: None,
            evidence_path: None,
            last_error_json: None,
            quarantine_id: None,
        };
        runtime.transition_readiness(&transition).unwrap();
        assert_eq!(
            runtime.test_source_records().len(),
            owner_count + 1,
            "scope escalation is not an exact retry"
        );
        runtime.transition_readiness(&transition).unwrap();
        assert_eq!(runtime.test_source_records().len(), owner_count + 1);
        runtime
            .rebuild_projection(
                &mut crate::projector::open(&paths.sqlite_path)
                    .unwrap()
                    .with_library_runtime(runtime.clone())
                    .write_handle(),
            )
            .unwrap();
        let row = runtime
            .reader()
            .unwrap()
            .media_readiness_operation(input.operation_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            row.dirty_scope.as_deref(),
            Some("selected-library-snapshot")
        );
        let records = runtime.test_source_records();
        let observations: Vec<_> = records
            .iter()
            .filter_map(|r| r.readiness_observation())
            .collect();
        assert_eq!(observations.len(), 2);
        assert_eq!(
            observations[0].started_unix_ns,
            observations[1].started_unix_ns
        );
        assert_eq!(observations[0].limit_ms, observations[1].limit_ms);
        let endpoint = lib::DriveKey::Bay(lib::Bay {
            library_serial: "LIB".into(),
            bay: 0x100,
        });
        let intent = |attempt| {
            runtime
                .append(|a, v| {
                    a.motion_intent(
                        lib::MotionIntent {
                            command: *uuid::Uuid::new_v4().as_bytes(),
                            parent_operation: Some(operation),
                            motion: lib::Motion::Load(lib::Load {
                                endpoint: endpoint.clone(),
                                attempt,
                            }),
                            tape: None,
                            inventory_generation: None,
                            readiness_generation: None,
                        },
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer2,
                        v,
                    )
                })
                .unwrap()
        };
        let old = intent(1);
        runtime
            .append(|a, v| {
                a.inventory_reconciled(
                    lib::InventoryReconciled {
                        subject: lib::InventorySubject::Library("LIB".into()),
                        observation_generation: 1,
                        resolves: lib::Bounded(vec![lib::DecisionRef {
                            sequence: old.sequence,
                            record_uuid: old.record_uuid,
                        }]),
                        complete: true,
                        elements: lib::Bounded(vec![]),
                    },
                    lib::AuditActor::System,
                    lib::SourceLayer::Layer2,
                    v,
                )
            })
            .unwrap();
        {
            let _dispatch = runtime.0.dispatch.lock().unwrap();
            assert!(runtime
                .pending_readiness_load_locked(operation, &endpoint)
                .unwrap()
                .is_none());
        }
        let current = intent(2);
        let mut ready = transition.clone();
        ready.dirty_scope = Some("none".into());
        ready.state = "ready".into();
        ready.phase = Some("ready".into());
        let before = runtime.test_source_records().len();
        let budget = runtime.readiness_load_attempts(operation).unwrap();
        assert!(
            runtime.transition_readiness(&ready).is_err(),
            "missing probe evidence must not reset budget"
        );
        let mut failed: lib::CompletionEvidence =
            (&remanence_library::outcome::CompletionEvidence::success()).into();
        failed.scsi_status = Some(2);
        ready.completion_evidence = Some(failed);
        assert!(
            runtime.transition_readiness(&ready).is_err(),
            "failed probe must not reset budget"
        );
        assert_eq!(runtime.test_source_records().len(), before);
        assert_eq!(runtime.readiness_load_attempts(operation).unwrap(), budget);
        let mut fake_initial = input.clone();
        fake_initial.state = "ready".into();
        assert!(runtime.begin_owned_readiness(&fake_initial).is_err());
        ready.completion_evidence =
            Some((&remanence_library::outcome::CompletionEvidence::success()).into());
        runtime.transition_readiness(&ready).unwrap();
        let records = runtime.test_source_records();
        let ready_source = records
            .iter()
            .filter_map(|r| r.readiness_observation())
            .next_back()
            .unwrap();
        assert_eq!(
            ready_source.load_intent,
            Some(lib::DecisionRef {
                sequence: current.sequence,
                record_uuid: current.record_uuid
            })
        );
        assert_eq!(runtime.readiness_load_attempts(operation).unwrap(), 0);
        let closed = runtime
            .start_owned_operation(
                lib::OperationKind::MediaReadinessWait,
                lib::AuditActor::System,
            )
            .unwrap();
        runtime
            .record_operation_event(
                closed,
                lib::OperationKind::MediaReadinessWait,
                lib::OperationEvent::CancelledBeforeDispatch(lib::Unit),
                lib::AuditActor::System,
            )
            .unwrap();
        forged.operation_id = uuid::Uuid::from_bytes(closed);
        let before = runtime.test_source_records().len();
        assert!(runtime.begin_owned_readiness(&forged).is_err());
        assert_eq!(runtime.test_source_records().len(), before);
    }
    /// Endpoint ownership follows observation order, and readiness freezes the
    /// selected serial before later replacement, retirement or bay movement.
    #[test]
    fn readiness_serial_is_proved_at_creation_and_survives_rebuild() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::StatePaths {
            config_path: root.path().join("config"),
            state_dir: root.path().join("state"),
            audit_dir: root.path().join("library"),
            journal_dir: root.path().join("native"),
            sqlite_path: root.path().join("catalog.sqlite"),
            tape_cache_dir: root.path().join("cache"),
            calibration_dir: root.path().join("calibration"),
        };
        let lease = Arc::new(crate::StateLockGuard::acquire(&paths.state_dir).unwrap());
        let runtime = LibraryRuntime::open(&paths, true, lease).unwrap();
        let observe = |serial: &str, bay| {
            runtime
                .observe_drives(vec![crate::DriveObservationInput {
                    serial: serial.into(),
                    identity_source: "DvcidInline".into(),
                    vendor: Some("V".into()),
                    product: Some("P".into()),
                    firmware_rev: None,
                    managed: "rem".into(),
                    library_serial: Some("LIB".into()),
                    element_address: Some(bay),
                    observed_at_utc: None,
                }])
                .unwrap()[0]
                .drive_uuid
                .clone()
        };
        let old = observe("OLD", 0x100);
        let current = observe("CURRENT", 0x100);
        assert_ne!(old, current);
        let operation = runtime
            .start_owned_operation(
                lib::OperationKind::MediaReadinessWait,
                lib::AuditActor::System,
            )
            .unwrap();
        let mut input = MediaReadinessOperationInput {
            operation_id: uuid::Uuid::from_bytes(operation),
            run_id: None,
            endpoint: crate::DriveKey::new("LIB", 0x100),
            changer_sg: None,
            drive_sg: None,
            drive_serial: Some("CURRENT".into()),
            barcode: None,
            source_slot: None,
            media_generation: None,
            media_family: None,
            phase: "readiness_poll".into(),
            state: "planned".into(),
            dirty_scope: None,
            deadline_at_utc: Some(
                (OffsetDateTime::now_utc() + time::Duration::seconds(60))
                    .format(&Rfc3339)
                    .unwrap(),
            ),
            evidence_path: None,
        };
        runtime.begin_owned_readiness(&input).unwrap();
        assert_eq!(
            observe("CURRENT", 0x101),
            current,
            "bay move preserves identity"
        );
        observe("REPLACEMENT", 0x100);
        runtime.begin_owned_readiness(&input).unwrap();
        runtime
            .rebuild_projection(
                &mut crate::projector::open(&paths.sqlite_path)
                    .unwrap()
                    .with_library_runtime(runtime.clone())
                    .write_handle(),
            )
            .unwrap();
        assert_eq!(
            runtime
                .reader()
                .unwrap()
                .media_readiness_operation(input.operation_id)
                .unwrap()
                .unwrap()
                .drive_serial
                .as_deref(),
            Some("CURRENT")
        );
        input.drive_serial = Some("REPLACEMENT".into());
        assert!(
            runtime.begin_owned_readiness(&input).is_err(),
            "retry cannot replace original serial"
        );
        let current_id = current.as_slice().try_into().unwrap();
        for (decision, expected, next, serial) in [
            (
                lib::Stewardship::Retire(lib::Reason {
                    reason: "operator".into(),
                }),
                1,
                2,
                None,
            ),
            (
                lib::Stewardship::Reinstate(lib::Reason {
                    reason: "operator".into(),
                }),
                2,
                3,
                Some("CURRENT"),
            ),
        ] {
            runtime
                .append(|a, v| {
                    a.drive_decision(
                        lib::DriveDecision {
                            drive_uuid: current_id,
                            decision,
                            change: lib::GenerationChange { expected, next },
                        },
                        expected,
                        lib::AuditActor::System,
                        lib::SourceLayer::Layer4,
                        v,
                    )
                })
                .unwrap();
            let _dispatch = runtime.0.dispatch.lock().unwrap();
            assert_eq!(
                runtime
                    .readiness_drive_serial_locked(&crate::DriveKey::new("LIB", 0x101), [99; 16])
                    .unwrap()
                    .as_deref(),
                serial
            );
        }
        let operation = runtime
            .start_owned_operation(
                lib::OperationKind::MediaReadinessWait,
                lib::AuditActor::System,
            )
            .unwrap();
        input.operation_id = uuid::Uuid::from_bytes(operation);
        input.endpoint = crate::DriveKey::Standalone {
            drive_serial: "STANDALONE".into(),
        };
        input.drive_serial = Some("STANDALONE".into());
        runtime.begin_owned_readiness(&input).unwrap();
        assert_eq!(
            runtime
                .reader()
                .unwrap()
                .media_readiness_operation(input.operation_id)
                .unwrap()
                .unwrap()
                .drive_serial
                .as_deref(),
            Some("STANDALONE")
        );
    }
}
