//! Transitional standalone write/lifecycle orchestration over shared mount support.

use crate::{PoolWriteResult, TapeUuid, WriteObjectToPoolRequest};
use remanence_library::{DriveHandle, DriveHandleSource};
use remanence_state::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSubject, SourceLayer, StateHandle,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

// Preserve public and crate-local mount paths until all migration commits land.
pub(crate) use crate::mount_support::fence_release_options;
use crate::mount_support::{raise_standalone_fence, refusal};
pub use crate::mount_support::{
    standalone_fence_policy, standalone_fence_releasable, ChangerMount, MountBackend,
    MountEndpoint, MountError, MountTarget, MountedTape, OperatorMount,
};

impl OperatorMount<'_> {
    /// Prepare a BOT initialization decision, then run its write and provisioning
    /// under the same durable calibration/session guards as mounted writes.
    /// Preparation must only inspect/configure media; it returns the planned UUID.
    pub fn initialize<T, R>(
        &mut self,
        reserve: bool,
        prepare: impl FnOnce(&mut DriveHandle, &mut StateHandle) -> Result<(TapeUuid, T), String>,
        write_and_provision: impl FnOnce(&mut DriveHandle, &mut StateHandle, T) -> Result<R, String>,
    ) -> Result<R, MountError> {
        let mut drive = self
            .context
            .drive
            .take()
            .ok_or_else(|| refusal("one cartridge per invocation"))?;
        if drive.endpoint().durable_key().is_none() {
            return Err(refusal("ephemeral endpoint is read-only"));
        }
        let mut reserved = false;
        let result = (|| {
            if reserve {
                drive.reserve().map_err(refusal)?;
                reserved = true;
            }
            let (uuid, plan) = prepare(&mut drive, self.state).map_err(refusal)?;
            self.flush_audit()?;
            standalone_fence_policy(
                self.state.catalog_index(),
                Some(uuid),
                drive.endpoint().durable_key(),
                true,
            )?;
            let control = self.state.calibration_control().clone();
            crate::calibration::install_standalone_calibration_fence(&mut drive, &control, uuid)
                .map_err(refusal)?;
            let id = uuid::Uuid::new_v4();
            self.session(uuid, id, AuditEvent::SessionOpened)?;
            let before = drive.media_write_dispatch_count();
            let result = write_and_provision(&mut drive, self.state, plan).map_err(refusal);
            self.flush_audit()?;
            let uncertain = result.is_err() && drive.media_write_dispatch_count() != before;
            if uncertain {
                self.unusable = true;
                raise_standalone_fence(
                    self.state,
                    uuid,
                    self.context.endpoint.durable_key(),
                    "standalone_init_failed",
                    &result.as_ref().err().expect("failed write").to_string(),
                )?;
            }
            self.session(
                uuid,
                id,
                if uncertain {
                    AuditEvent::SessionOrphaned
                } else {
                    AuditEvent::SessionClosed
                },
            )?;
            result
        })();
        let allow = drive.allow_medium_removal().map_err(refusal);
        let release = if reserved {
            drive.release_reservation().map_err(refusal)
        } else {
            Ok(())
        };
        self.flush_audit()?;
        let value = result?;
        allow?;
        release?;
        Ok(value)
    }

    pub(crate) fn session(
        &mut self,
        tape_uuid: TapeUuid,
        id: uuid::Uuid,
        event: AuditEvent,
    ) -> Result<(), MountError> {
        self.flush_audit()?;
        let key = self
            .context
            .endpoint
            .durable_key()
            .ok_or_else(|| refusal("ephemeral endpoint is read-only"))?;
        let detail = BTreeMap::from([
            ("session_kind".into(), ciborium::Value::Text("write".into())),
            (
                "tape_uuid".into(),
                ciborium::Value::Bytes(tape_uuid.to_vec()),
            ),
            ("endpoint_key".into(), ciborium::Value::Text(key.encode())),
        ]);
        // Session recovery evidence must reach stable storage before any write,
        // even when ordinary audit appends are configured without fsync.
        use remanence_state::AuditSink;
        let mut log = remanence_state::FileAuditLog::open(&self.state.paths().audit_dir, true)
            .map_err(refusal)?;
        log.append(AuditEventRecord {
            event,
            actor: AuditActor::local_user(),
            source_layer: SourceLayer::Layer5,
            session_id: Some(id),
            operation_id: None,
            idempotency_key: None,
            subject: AuditSubject {
                kind: "write".into(),
                id: Some(id.to_string()),
            },
            detail,
        })
        .map_err(refusal)?;
        self.state.refresh_audit_append_cursor().map_err(refusal)?;
        self.state.replay_audit_projection().map_err(refusal)?;
        Ok(())
    }
    /// Reconcile durable checkpoint authority before any standalone catalog decision.
    pub fn reconcile_checkpoints(&mut self) -> Result<(), MountError> {
        if !self.checkpoints_reconciled {
            crate::reconcile_checkpoint_journal_projections(self.state).map_err(refusal)?;
            self.checkpoints_reconciled = true;
        }
        Ok(())
    }

    /// Decide a standalone file version, then wrap the ordinary admitted writer.
    /// Preparation is reused so mutations during streaming fail writer validation.
    pub fn put_path_version(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
    ) -> Result<Result<(PoolWriteResult, &'static str), remanence_state::VersionRow>, MountError>
    {
        self.put_path_version_with_policy(mounted, request, false)
    }

    /// Explicit plaintext downgrade policy around the same version and write funnel.
    pub fn put_path_version_with_policy(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
        allow_plaintext_copy: bool,
    ) -> Result<Result<(PoolWriteResult, &'static str), remanence_state::VersionRow>, MountError>
    {
        self.put_path_version_with_preparation(mounted, request, allow_plaintext_copy, || {})
    }

    /// Keep the decision and preparation in one funnel; the hook models a source race.
    pub(crate) fn put_path_version_with_preparation(
        &mut self,
        mounted: &mut MountedTape,
        mut request: WriteObjectToPoolRequest,
        allow_plaintext_copy: bool,
        before_prepare: impl FnOnce(),
    ) -> Result<Result<(PoolWriteResult, &'static str), remanence_state::VersionRow>, MountError>
    {
        self.require_durable_mount(mounted)?;
        self.reconcile_checkpoints()?;
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        let path =
            remanence_stream::normalize_archive_path(&request.archive_path).map_err(refusal)?;
        request.archive_path = PathBuf::from(&path);
        if !allow_plaintext_copy
            && matches!(
                request.representation,
                crate::PoolWriteRepresentation::Plaintext
            )
        {
            if let Some(version) = self
                .state
                .catalog_index()
                .list_committed_versions_by_path_any_pool(&path)
                .map_err(refusal)?
                .first()
            {
                let record = self
                    .state
                    .catalog_index()
                    .get_native_object(&version.object_id)
                    .map_err(refusal)?
                    .ok_or_else(|| refusal("version object disappeared"))?;
                if record
                    .copies
                    .iter()
                    .any(|copy| copy.status == "committed" && copy.representation == "encrypted")
                {
                    return Err(MountError::PlaintextCopy {
                        newest_version: version.object_id.clone(),
                    });
                }
            }
        }
        let newest = self
            .state
            .catalog_index()
            .list_committed_versions_by_path(&request.pool_id, &path)
            .map_err(refusal)?
            .into_iter()
            .next();
        let representation_matches = if let Some(version) = &newest {
            let record = self
                .state
                .catalog_index()
                .get_native_object(&version.object_id)
                .map_err(refusal)?
                .ok_or_else(|| refusal("version object disappeared"))?;
            record.copies.iter().any(|copy| {
                copy.status == "committed"
                    && copy.pool_id.as_deref() == Some(request.pool_id.as_str())
                    && match &request.representation {
                        crate::PoolWriteRepresentation::Plaintext => {
                            copy.representation == "plaintext"
                        }
                        crate::PoolWriteRepresentation::Encrypted { recipients } => {
                            let requested: std::collections::BTreeSet<_> = recipients
                                .iter()
                                .map(|r| {
                                    r.recipient_epoch_id
                                        .iter()
                                        .map(|b| format!("{b:02x}"))
                                        .collect::<String>()
                                })
                                .collect();
                            copy.representation == "encrypted"
                                && copy.recipient_epoch_ids.as_ref().is_some_and(|ids| {
                                    ids.iter()
                                        .cloned()
                                        .collect::<std::collections::BTreeSet<_>>()
                                        == requested
                                })
                        }
                    }
            })
        } else {
            false
        };
        let reason = if newest.is_none() {
            "new"
        } else if !representation_matches {
            "representation_changed"
        } else {
            "content_changed"
        };
        if let Some(version) = &newest {
            if representation_matches
                && request.source.size_bytes().map_err(refusal)? == version.size_bytes
                && request.source.content_sha256().map_err(refusal)?.as_slice()
                    == version.file_sha256
            {
                return Ok(Err(version.clone()));
            }
        }
        let crate::WriteObjectSource::Path(source) = &request.source else {
            return Err(refusal("standalone put requires a regular file path"));
        };
        before_prepare();
        let prepared = remanence_stream::prepare_regular_file(
            source,
            &request.archive_path,
            uuid::Uuid::new_v4().to_string(),
        )
        .map_err(refusal)?;
        if let Some(version) = newest {
            if representation_matches
                && prepared.spec.size_bytes == version.size_bytes
                && prepared.spec.file_sha256.as_ref().map(|d| d.as_slice())
                    == Some(version.file_sha256.as_slice())
            {
                return Ok(Err(version));
            }
        }
        request.source =
            crate::WriteObjectSource::PreparedPath(crate::pool_write::PreparedPathSource(prepared));
        self.write_checkpointed(mounted, request)
            .map(|result| Ok((result, reason)))
    }

    /// Durable session and calibration fencing wrap the existing admission funnel.
    pub fn write_checkpointed(
        &mut self,
        mounted: &mut MountedTape,
        request: WriteObjectToPoolRequest,
    ) -> Result<PoolWriteResult, MountError> {
        self.require_durable_mount(mounted)?;
        self.reconcile_checkpoints()?;
        self.flush_audit()?;
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        let pool = self
            .state
            .config()
            .tape_pools
            .iter()
            .find(|p| p.id == request.pool_id)
            .cloned()
            .ok_or_else(|| refusal("unconfigured pool"))?;
        let loaded = self
            .state
            .catalog_index()
            .get_tape(&mounted.tape_uuid)
            .map_err(refusal)?
            .ok_or_else(|| refusal("mounted tape is not catalogued"))?;
        if loaded.pool_id.as_deref() != Some(request.pool_id.trim()) {
            return Err(refusal("requested pool must equal the loaded tape's pool"));
        }
        if let Some(replay) =
            crate::pool_write::replay_committed_pool_write_reconciled(self.state, &request)
                .map_err(|e| MountError::PoolWrite(Box::new(e)))?
        {
            return Ok(replay);
        }
        let journals = self.state.paths().journal_dir.join("checkpoints");
        let disposition = crate::pool_write::admit_pinned_tape_for_write_session(
            self.state.catalog_index(),
            mounted.tape_uuid,
            &request.pool_id,
            &pool,
            &journals,
        )
        .map_err(|error| match error {
            crate::pool_write::PinnedTapeError::NotWritable {
                reason:
                    crate::pool_write::WritabilityError::InsufficientCapacity {
                        object_size,
                        raw_capacity,
                        used,
                    },
                ..
            } => MountError::PoolWrite(Box::new(
                crate::PoolWriteError::SelectedTapeInsufficientCapacity {
                    object_size,
                    raw_capacity,
                    used,
                },
            )),
            crate::pool_write::PinnedTapeError::NotWritable { .. }
                if loaded
                    .terminal_finalization
                    .as_ref()
                    .is_some_and(|terminal| {
                        terminal.trigger
                            == remanence_state::TerminalFinalizationTrigger::HardwareEarlyWarning
                    }) =>
            {
                MountError::EarlyWarning
            }
            other => refusal(other),
        })?;
        let selected = match disposition {
            crate::pool_write::PinnedWriteDisposition::Writable(selected) => selected,
            _ => {
                return Err(MountError::PoolWrite(Box::new(
                    crate::PoolWriteError::TerminalCloseRequired {
                        detail: "tape requires terminal recovery before writing".into(),
                    },
                )))
            }
        };
        let id = uuid::Uuid::new_v4();
        self.session(mounted.tape_uuid, id, AuditEvent::SessionOpened)?;
        let before = mounted.drive.media_write_dispatch_count();
        let result = (|| {
            mounted.drive.test_unit_ready().map_err(refusal)?;
            mounted.drive.prevent_medium_removal().map_err(refusal)?;
            crate::pool_write::write_to_selected_drive_checkpointed_reconciled(
                self.state,
                &mut mounted.drive,
                request,
                selected,
            )
            .map_err(|e| MountError::PoolWrite(Box::new(e)))
        })();
        let uncertain = result.is_err() && mounted.drive.media_write_dispatch_count() != before;
        if uncertain {
            self.unusable = true;
            raise_standalone_fence(
                self.state,
                mounted.tape_uuid,
                mounted.endpoint.durable_key(),
                "standalone_write_failed",
                &result.as_ref().expect_err("failed write").to_string(),
            )?;
        }
        let closed = self.session(
            mounted.tape_uuid,
            id,
            if uncertain {
                AuditEvent::SessionOrphaned
            } else {
                AuditEvent::SessionClosed
            },
        );
        if closed.is_err() && mounted.drive.media_write_dispatch_count() != before {
            self.unusable = true;
        }
        closed?;
        if result.as_ref().is_ok_and(|r| !r.is_replay()) {
            mounted.identity_generation = mounted.drive.identity_generation();
        }
        result
    }
}
/// Operator acknowledgements are checked before the shared finalization admission.
pub struct StandaloneFinalizeRequest {
    pub tape_uuid: TapeUuid,
    pub acknowledge_tape: TapeUuid,
    pub expected_pool_id: Option<String>,
    pub reason: String,
    pub idempotency_key: uuid::Uuid,
}
impl OperatorMount<'_> {
    /// Close a partially filled tape through the daemon's terminal safeguards.
    pub fn finalize(
        &mut self,
        mounted: &mut MountedTape,
        request: StandaloneFinalizeRequest,
    ) -> Result<remanence_state::TerminalFinalizationProjection, MountError> {
        self.require_durable_mount(mounted)?;
        if request.tape_uuid != mounted.tape_uuid || request.acknowledge_tape != mounted.tape_uuid {
            return Err(refusal(
                "--tape and --acknowledge-tape must match the mounted BOT UUID",
            ));
        }
        if request.reason.trim().is_empty()
            || request.idempotency_key.is_nil()
            || request
                .expected_pool_id
                .as_ref()
                .is_some_and(|p| p.is_empty() || p.trim() != p)
        {
            return Err(refusal(
                "finalize requires a reason, non-nil idempotency key and canonical expected pool",
            ));
        }
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        let actor = AuditActor::local_user();
        let actor_fingerprint = crate::catalog_conversion::audit_actor_fingerprint(&actor);
        let fingerprint = crate::catalog_conversion::manual_finalize_request_fingerprint(
            request.tape_uuid,
            request.expected_pool_id.as_deref(),
            &actor_fingerprint,
            request.reason.as_bytes(),
        );
        let pools = self.state.config().tape_pools.clone();
        let (tape, mut request) = crate::mount::prepare_manual_finalize_request(
            self.state.catalog_index(),
            crate::mount::ManualFinalizeTapeAdmission {
                candidate_operation_id: uuid::Uuid::new_v4(),
                actor,
                actor_fingerprint,
                idempotency_key: request.idempotency_key,
                request_fingerprint: fingerprint,
                tape_uuid: request.tape_uuid,
                expected_pool_id: request.expected_pool_id,
                reason: request.reason,
            },
            |id| {
                pools
                    .iter()
                    .find(|p| p.id == id)
                    .cloned()
                    .ok_or_else(|| tonic::Status::failed_precondition("unconfigured pool"))
            },
        )
        .map_err(refusal)?;
        self.flush_audit()?;
        let cfg = crate::write_owner::TerminalWriteConfig {
            checkpoint_journal_dir: self.state.paths().journal_dir.join("checkpoints"),
            audit_dir: self.state.paths().audit_dir.clone(),
            audit_fsync: self.state.config().audit.fsync,
            audit_append_lock: Arc::new(Mutex::new(())),
        };
        let session_id = uuid::Uuid::new_v4();
        self.session(mounted.tape_uuid, session_id, AuditEvent::SessionOpened)?;
        let result = (|| {
            let pending = remanence_state::FileCheckpointJournal::open(
                &cfg.checkpoint_journal_dir,
                request.tape_uuid,
            )
            .and_then(|journal| journal.terminal_finalization_intent())
            .map_err(crate::status_from_state_error)?;
            let (prefix, carried) = if pending.is_none()
                && tape.scheme_id.is_some()
                && tape.highest_protected_ordinal < tape.total_committed_ordinals
            {
                let (plan, state) = crate::write_owner::plan_unretained_epoch_finalization(
                    self.state.catalog_index(),
                    &cfg,
                    &mut mounted.drive,
                    &request,
                )?;
                (
                    Some(plan),
                    state.map(|state| (mounted.drive.identity_generation(), state)),
                )
            } else {
                (None, None)
            };
            if let Some(reply) = crate::write_owner::preflight_manual_finalize_with_prefix(
                self.state.catalog_index(),
                crate::write_owner::ManualFinalizePreflightConfig {
                    checkpoint_journal_dir: &cfg.checkpoint_journal_dir,
                    audit_dir: &cfg.audit_dir,
                    audit_fsync: cfg.audit_fsync,
                    audit_append_lock: &cfg.audit_append_lock,
                },
                tape.voltag.as_deref(),
                &mut request,
                prefix.as_ref(),
            )? {
                return Ok(reply);
            }
            crate::write_owner::finalize_mounted_tape(
                self.state.catalog_index(),
                &cfg,
                &mut mounted.drive,
                &request,
                tape.voltag.as_deref(),
                carried,
                |_, drive| {
                    drive
                        .test_unit_ready()
                        .map_err(|e| tonic::Status::unavailable(e.to_string()))?;
                    drive
                        .prevent_medium_removal()
                        .map_err(|e| tonic::Status::unavailable(e.to_string()))?;
                    crate::write_owner::prepare_drive_for_write(
                        drive,
                        &request.tape_uuid,
                        request.block_size,
                        request.candidate_operation_id,
                        crate::write_owner::WriteMediaPolicy::TerminalAppend,
                    )
                },
                |_, _| {},
            )
        })();
        // Terminal routines use their existing synchronized audit appender.
        self.state.refresh_audit_append_cursor().map_err(refusal)?;
        self.flush_audit()?;
        self.session(
            mounted.tape_uuid,
            session_id,
            if result.is_ok() {
                AuditEvent::SessionClosed
            } else {
                AuditEvent::SessionOrphaned
            },
        )?;
        result.map(|reply| reply.projection).map_err(refusal)
    }
}

impl OperatorMount<'_> {
    /// Discard an explicitly acknowledged suffix through the shared drive-owner path.
    pub fn supersede_tail(
        &mut self,
        mounted: &mut MountedTape,
        acknowledge: &str,
        confirm: impl FnOnce(&str, &[String]) -> Result<(), String>,
    ) -> Result<Vec<String>, MountError> {
        self.require_durable_mount(mounted)?;
        standalone_fence_policy(
            self.state.catalog_index(),
            Some(mounted.tape_uuid),
            mounted.endpoint.durable_key(),
            true,
        )?;
        if mounted.generation.is_none() {
            return Err(refusal(
                "tape generation is required before superseding a tail",
            ));
        }
        self.flush_audit()?;
        let cfg = crate::write_owner::TerminalWriteConfig {
            checkpoint_journal_dir: self.state.paths().journal_dir.join("checkpoints"),
            audit_dir: self.state.paths().audit_dir.clone(),
            audit_fsync: true,
            audit_append_lock: Arc::new(Mutex::new(())),
        };
        let session_id = uuid::Uuid::new_v4();
        self.session(mounted.tape_uuid, session_id, AuditEvent::SessionOpened)?;
        let before = mounted.drive.media_write_dispatch_count();
        let result = supersede_mounted_tail(
            self.state.catalog_index(),
            &cfg,
            &mut mounted.drive,
            mounted.tape_uuid,
            mounted.identity_generation,
            acknowledge,
            AuditActor::local_user(),
            confirm,
        );
        let uncertain = result.is_err() && before != mounted.drive.media_write_dispatch_count();
        self.unusable |= uncertain;
        self.session(
            mounted.tape_uuid,
            session_id,
            if uncertain {
                AuditEvent::SessionOrphaned
            } else {
                AuditEvent::SessionClosed
            },
        )?;
        self.state.refresh_audit_append_cursor().map_err(refusal)?;
        result.map_err(refusal)
    }
}

/// Explicit recovery shared by standalone ownership and the library drive actor.
/// Confirmation, rescan, audit, and mutation all retain the same exclusive lease.
#[allow(clippy::too_many_arguments)]
pub(crate) fn supersede_mounted_tail(
    index: &mut remanence_state::CatalogIndex,
    cfg: &crate::write_owner::TerminalWriteConfig,
    drive: &mut DriveHandle,
    tape_uuid: [u8; 16],
    identity_generation: u64,
    acknowledge: &str,
    actor: AuditActor,
    confirm: impl FnOnce(&str, &[String]) -> Result<(), String>,
) -> Result<Vec<String>, tonic::Status> {
    use crate::append_admission::{
        admit_append_boundary, context, AdmissionError, AppendAuthority,
    };
    use tonic::Status;
    let failure = |e: String| Status::failed_precondition(e);
    let tape = index
        .get_tape(&tape_uuid)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| failure("adopt the tape before superseding its tail".into()))?;
    if tape.cartridge_generation.is_none() {
        return Err(failure("tape generation is required".into()));
    }
    if tape.terminal_finalization.is_some() {
        return Err(failure("terminal finalization prevents tail discard; retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent".into()));
    }
    if tape.kind != "data"
        || !matches!(tape.state.as_str(), "ready" | "recovery_required")
        || tape.terminal_finalization.is_some()
    {
        return Err(failure(
            "tape is sealed, retired, or requires terminal recovery".into(),
        ));
    }
    if !index
        .tape_io_admission_conflicts(&tape_uuid, tape.voltag.as_deref())
        .map_err(crate::status_from_state_error)?
        .is_empty()
    {
        return Err(failure(
            "release the tape I/O fence before superseding".into(),
        ));
    }
    let uuid = uuid::Uuid::from_bytes(tape_uuid).to_string();
    let label = tape.voltag.as_deref().unwrap_or(&uuid);
    if acknowledge != uuid && acknowledge != label {
        return Err(failure(
            "acknowledgment does not identify the loaded tape".into(),
        ));
    }
    let (size, parity) = crate::pool_write::selected_tape_geometry(
        &tape,
        tape.pool_id.as_deref().unwrap_or("(unpooled)"),
    )
    .map_err(|e| failure(e.to_string()))?;
    let journal =
        remanence_state::FileCheckpointJournal::open(&cfg.checkpoint_journal_dir, tape_uuid)
            .map_err(crate::status_from_state_error)?;
    let terminal_guidance = "retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent";
    if journal
        .terminal_finalization_intent()
        .map_err(|e| {
            failure(format!(
                "cannot establish terminal authority: {e}; {terminal_guidance}"
            ))
        })?
        .is_some()
    {
        return Err(failure(format!(
            "terminal finalization intent prevents tail discard; {terminal_guidance}"
        )));
    }
    let lease = journal
        .acquire_exclusive()
        .map_err(crate::status_from_state_error)?;
    if lease
        .terminal_finalization_intent()
        .map_err(crate::status_from_state_error)?
        .is_some()
    {
        return Err(failure(
            "terminal finalization intent prevents tail discard; retry FinalizeTape with its original idempotency key. If rolled-back state cannot resume that intent, restore a newer copy of the state directory; supersede-tail cannot clear a stale terminal intent".into(),
        ));
    }
    let last = lease
        .last_record_bounded()
        .map_err(crate::status_from_state_error)?;
    let make_context = || {
        let mut ctx = context(
            tape_uuid,
            size,
            last.as_ref().map_or(0, |r| r.eod_partition),
            last.as_ref().map_or(0, |r| r.eod_lba),
        );
        ctx.inspect_all = true;
        ctx
    };
    let authority = || {
        last.as_ref()
            .map_or(AppendAuthority::Fresh, |_| AppendAuthority::Checkpoint {
                eod: make_context().boundary,
            })
    };
    drive
        .bind_identity_generation(identity_generation)
        .map_err(|e| failure(e.to_string()))?;
    drive.rewind().map_err(|e| failure(e.to_string()))?;
    crate::verify_tape_identity(&mut DriveHandleSource(drive), &tape_uuid)
        .map_err(|e| failure(e.to_string()))?;
    let config = drive.read_config().map_err(|e| failure(e.to_string()))?;
    crate::pool_write::require_rewritable_object_media(config)
        .map_err(|e| failure(e.to_string()))?;
    crate::drive_mode::configure_fixed_uncompressed_write(drive, config, size)
        .map_err(|e| failure(e.to_string()))?;
    let (found, object_ids, missing_filemark) =
        match admit_append_boundary(drive, authority(), make_context()) {
            Err(AdmissionError::TailBeyondKnownState {
                found, object_ids, ..
            }) => (found, object_ids, false),
            Err(AdmissionError::MissingBootstrapFilemark) => (Vec::new(), Vec::new(), true),
            Err(e) => return Err(failure(e.to_string())),
            Ok(_) => return Err(failure("no foreign tail requires superseding".into())),
        };
    confirm(&format!("SUPERSEDE {label}"), &found).map_err(failure)?;
    let mut ctx = make_context();
    ctx.confirmed_discard = Some(found.clone());
    ctx.allow_missing_bootstrap_filemark = missing_filemark;
    let _admission =
        admit_append_boundary(drive, authority(), ctx).map_err(|e| failure(e.to_string()))?;
    drive
        .bind_identity_generation(identity_generation)
        .map_err(|e| failure(e.to_string()))?;
    crate::audit_projection::append_and_project_audit(
        index,
        &cfg.audit_dir,
        true,
        &cfg.audit_append_lock,
        crate::audit_projection::ProjectedAuditInput {
            actor: actor.clone(),
            source_layer: SourceLayer::Layer5,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: AuditEvent::RecoveryEvent,
            subject_kind: "tape",
            subject_id: Some(uuid.clone()),
            detail: BTreeMap::from([
                (
                    "action".into(),
                    ciborium::Value::Text("supersede_tail".into()),
                ),
                (
                    "discarded_object_ids".into(),
                    ciborium::Value::Array(
                        object_ids.into_iter().map(ciborium::Value::Text).collect(),
                    ),
                ),
                (
                    "found".into(),
                    ciborium::Value::Array(
                        found.iter().cloned().map(ciborium::Value::Text).collect(),
                    ),
                ),
                (
                    "expected_eod_lba".into(),
                    ciborium::Value::Integer(make_context().boundary.lba.into()),
                ),
            ]),
        },
    )?;
    let result = (|| {
        let mut sink = remanence_library::DriveHandleSink(drive);
        if last.is_none() {
            let bootstrap = crate::pool_write::build_tape_bootstrap(
                tape_uuid,
                size,
                parity.clone(),
                remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION"))
                    .written_at()
                    .map_err(|e| e.to_string())?,
                env!("CARGO_PKG_VERSION"),
            );
            crate::pool_write::write_tape_bootstrap(&mut sink, &bootstrap)
                .map_err(|e| e.to_string())?;
        } else {
            remanence_library::BlockSink::write_filemarks(&mut sink, 1)
                .map_err(|e| e.to_string())?;
        }
        if let remanence_parity::ParityConfig::Scheme(scheme) = &parity {
            let dir = cfg
                .checkpoint_journal_dir
                .parent()
                .ok_or("missing parity journal directory")?;
            let path = dir.join(format!("{}.remjournal", crate::bytes_to_hex(&tape_uuid)));
            let mut journal =
                remanence_parity::FileTapeFileJournal::open(path, tape_uuid, size, scheme.clone())
                    .map_err(|e| e.to_string())?;
            crate::append_admission::discard_reconciled_parity_suffix(&mut journal, &lease)?;
        }
        Ok::<_, String>(())
    })();
    drive.clear_append_admission();
    if let Err(error) = result {
        crate::write_owner::raise_tape_io_fence(
            index,
            cfg,
            remanence_state::TapeIoFenceInput {
                tape_uuid,
                barcode: tape.voltag,
                reason: "supersede_tail_failed".into(),
                evidence_json: Some(serde_json::json!({"error":error}).to_string()),
            },
            drive
                .endpoint()
                .durable_key()
                .filter(|key| matches!(key, remanence_library::DriveKey::Standalone { .. })),
        )
        .map_err(crate::status_from_state_error)?;
        return Err(Status::unavailable(error));
    }
    index
        .set_append_recovery_required(tape_uuid, false)
        .map_err(|e| {
            Status::failed_precondition(format!(
                "tail discard completed, but clearing tape recovery state failed: {e}"
            ))
        })?;
    Ok(found)
}
