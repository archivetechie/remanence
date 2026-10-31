//! Mounted one-shot commands compose today's admission, finalization and recovery primitives.

use super::{
    actor_runtime::{record_session_event, SessionAuditInput},
    terminal_types::TerminalConfig,
    WriteOwnerConfig,
};
use remanence_library::DriveHandle;
use remanence_state::authority::TapeAuthorityView;
use remanence_state::{AuditEvent, CatalogIndex};
use tonic::Status;
use uuid::Uuid;

pub(super) fn open_audit(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    id: Uuid,
) -> Result<(), Status> {
    let mounted = cfg
        .mounted()
        .ok_or_else(|| Status::failed_precondition("mounted command requires mounted ownership"))?;
    if let Some(error) = mounted
        .audit_error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
    {
        return Err(Status::internal(format!("drive audit failed: {error}")));
    }
    event(index, cfg, id, AuditEvent::SessionOpened)
}
/// Record successful media work even when open returns a terminal-close refusal.
pub(super) fn close_audit(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    id: Uuid,
) -> Result<(), Status> {
    event(index, cfg, id, AuditEvent::SessionClosed)
}
fn event(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    id: Uuid,
    event: AuditEvent,
) -> Result<(), Status> {
    let mounted = cfg.mounted().expect("mounted audit");
    record_session_event(
        index,
        cfg,
        SessionAuditInput {
            session_id: id,
            session_kind: "write",
            event,
            tape_uuid: Some(mounted.tape_uuid),
            library_serial: None,
            drive_bay: None,
            drive_uuid: None,
            drive_serial: match &mounted.endpoint {
                remanence_library::DriveKey::Standalone { drive_serial } => {
                    Some(drive_serial.clone())
                }
                _ => None,
            },
            abort_reason: None,
        },
    )
}

/// Settle an open whose caller disappeared or whose command failed before normal close.
pub(super) fn settle_audit(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &DriveHandle,
    before: u64,
) -> Result<(), Status> {
    let Some(mounted) = cfg.mounted() else {
        return Ok(());
    };
    let id = *mounted
        .audit_session
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(id) = id else {
        return Ok(());
    };
    let uncertain = before != drive.media_write_dispatch_count();
    // Explicit tail recovery already records its own durable invocation fence.
    // Preserve its single-fence contract; append failures retain both layers.
    let supersede_fenced = index
        .tape_io_admission_conflicts(&mounted.tape_uuid, mounted.barcode.as_deref())
        .map_err(crate::status_from_state_error)?
        .iter()
        .any(|fence| fence.reason == "supersede_tail_failed");
    if uncertain && !supersede_fenced {
        remanence_state::fence::raise(
            index,
            &cfg.audit_dir,
            remanence_state::TapeIoFenceInput {
                tape_uuid: mounted.tape_uuid,
                barcode: mounted.barcode.clone(),
                reason: "standalone_write_failed".into(),
                evidence_json: Some(
                    "{\"reason\":\"mounted operation ended without durable close\"}".into(),
                ),
            },
            Some(&mounted.endpoint),
            mounted.actor.clone(),
        )
        .map_err(crate::status_from_state_error)?;
    }
    event(
        index,
        cfg,
        id,
        if uncertain {
            AuditEvent::SessionOrphaned
        } else {
            AuditEvent::SessionClosed
        },
    )
}

pub(super) fn with_evidence<T>(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    operation: impl FnOnce(&mut CatalogIndex, &mut DriveHandle) -> Result<T, Status>,
) -> Result<T, Status> {
    let id = Uuid::new_v4();
    open_audit(index, cfg, id)?;
    let before = drive.media_write_dispatch_count();
    let result =
        operation(index, drive).and_then(|value| cfg.mounted_audit_ready().map(|()| value));
    let audit = if result.is_ok() {
        event(index, cfg, id, AuditEvent::SessionClosed)
    } else {
        settle_audit(index, cfg, drive, before)
    };
    if let Err(error) = &audit {
        *cfg.mounted()
            .expect("mounted evidence")
            .audit_error
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
    }
    match (result, audit) {
        (Err(primary), Err(audit)) => Err(super::ActorFailure::preserve(
            &primary,
            Status::new(
                primary.code(),
                format!(
                    "{}; additionally failed to settle session audit: {}",
                    primary.message(),
                    audit.message()
                ),
            ),
        )),
        (Err(error), Ok(())) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error),
    }
}

pub(super) fn close_epoch(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    bay: u16,
    retained: &mut Option<super::checkpoint::RetainedParityEpoch>,
) -> Result<(), Status> {
    cfg.mounted_ready(index, drive)?;
    // The shared close opens evidence only after journal authority proves an
    // epoch needs media work. A no-op finish must not create an orphan candidate.
    super::checkpoint::close_retained_before_removal(index, drive, cfg, bay, retained, None)
}

pub(super) fn finalize(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    input: crate::StandaloneFinalizeRequest,
) -> Result<remanence_state::TerminalFinalizationProjection, Status> {
    let mounted = cfg
        .mounted()
        .ok_or_else(|| Status::failed_precondition("mounted command requires mounted ownership"))?;
    if input.tape_uuid != mounted.tape_uuid || input.acknowledge_tape != mounted.tape_uuid {
        return Err(Status::failed_precondition(
            "--tape and --acknowledge-tape must match the mounted BOT UUID",
        ));
    }
    if input.reason.trim().is_empty()
        || input.idempotency_key.is_nil()
        || input
            .expected_pool_id
            .as_ref()
            .is_some_and(|p| p.is_empty() || p.trim() != p)
    {
        return Err(Status::invalid_argument(
            "finalize requires a reason, non-nil idempotency key and canonical expected pool",
        ));
    }
    cfg.mounted_admission(index, drive)?;
    let actor_fingerprint = crate::catalog_conversion::audit_actor_fingerprint(&mounted.actor);
    let request_fingerprint = crate::catalog_conversion::manual_finalize_request_fingerprint(
        input.tape_uuid,
        input.expected_pool_id.as_deref(),
        &actor_fingerprint,
        input.reason.as_bytes(),
    );

    let (tape, mut request) = crate::mount::prepare_manual_finalize_request(
        index,
        crate::mount::ManualFinalizeTapeAdmission {
            candidate_operation_id: Uuid::new_v4(),
            actor: mounted.actor.clone(),
            actor_fingerprint,
            idempotency_key: input.idempotency_key,
            request_fingerprint,
            tape_uuid: input.tape_uuid,
            expected_pool_id: input.expected_pool_id,
            reason: input.reason,
        },
        |id| {
            cfg.tape_pools
                .iter()
                .find(|p| p.id == id)
                .cloned()
                .ok_or_else(|| Status::failed_precondition("unconfigured pool"))
        },
    )?;
    with_evidence(index, cfg, drive, |index, drive| {
        let terminal = cfg.terminal_config();
        let pending = remanence_state::TapeAuthority::open(
            &terminal.checkpoint_journal_dir,
            request.tape_uuid,
            index.path(),
        )
        .and_then(|journal| journal.pending_finalization())
        .map_err(crate::status_from_state_error)?;
        let (prefix, carried) = if pending.is_none()
            && tape.scheme_id.is_some()
            && tape.highest_protected_ordinal < tape.total_committed_ordinals
        {
            cfg.mounted_ready(index, drive)?;
            let (plan, state) = super::terminal_finalize::plan_unretained_epoch_finalization(
                index, &terminal, drive, &request,
            )?;
            (
                Some(plan),
                state.map(|state| (drive.identity_generation(), state)),
            )
        } else {
            (None, None)
        };

        if let Some(reply) = super::preflight_manual_finalize_with_prefix(
            index,
            super::ManualFinalizePreflightConfig {
                mounted_endpoint: Some(&mounted.endpoint),
                checkpoint_journal_dir: &terminal.checkpoint_journal_dir,
                audit_dir: &terminal.audit_dir,
                audit_fsync: terminal.audit_fsync,
                audit_append_lock: &terminal.audit_append_lock,
            },
            tape.voltag.as_deref(),
            &mut request,
            prefix.as_ref(),
        )? {
            return Ok(reply.projection);
        }

        super::terminal_finalize::finalize_mounted_tape(
            index,
            &terminal,
            drive,
            &request,
            tape.voltag.as_deref(),
            carried,
            |index, drive| {
                cfg.mounted_ready(index, drive)?;
                super::write_session::prepare_drive_for_write(
                    drive,
                    &request.tape_uuid,
                    request.block_size,
                    request.candidate_operation_id,
                    super::write_session::WriteMediaPolicy::TerminalAppend,
                )
            },
            |_, _| {},
        )
        .map(|reply| reply.projection)
    })
}

pub(super) fn supersede(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    drive: &mut DriveHandle,
    request: crate::pb::SupersedeTailRequest,
) -> Result<crate::pb::SupersedeTailResponse, Status> {
    let mounted = cfg
        .mounted()
        .ok_or_else(|| Status::failed_precondition("mounted command requires mounted ownership"))?;
    if request.tape_uuid != mounted.tape_uuid {
        return Err(Status::failed_precondition(
            "supersede must name mounted tape",
        ));
    }
    if request.ack_tape_uuid != mounted.tape_uuid {
        return Err(Status::invalid_argument(
            "supersede requires acknowledgement",
        ));
    }
    if mounted.cartridge_generation.is_none() {
        return Err(Status::failed_precondition(
            "tape generation is required before superseding a tail",
        ));
    }
    cfg.mounted_ready(index, drive)?;
    with_evidence(index, cfg, drive, |index, drive| {
        let tape = index
            .get_tape(&mounted.tape_uuid)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("tape not found"))?;
        let (size, _) = crate::pool_write::selected_tape_geometry(
            &tape,
            tape.pool_id.as_deref().unwrap_or("(unpooled)"),
        )
        .map_err(|e| Status::failed_precondition(e.to_string()))?;
        super::write_session::prepare_drive_for_write(
            drive,
            &mounted.tape_uuid,
            size,
            Uuid::new_v4(),
            super::write_session::WriteMediaPolicy::RewritableObject,
        )?;
        let generation = drive.identity_generation();
        let mut preview = None;
        let result = crate::write_owner::supersede_mounted_tail(
            index,
            &cfg.terminal_config(),
            drive,
            mounted.tape_uuid,
            generation,
            &Uuid::from_bytes(mounted.tape_uuid).to_string(),
            mounted.actor.clone(),
            |required, found| {
                preview = Some(crate::pb::SupersedeTailResponse {
                    found: found.to_vec(),
                    required_confirmation: required.into(),
                    completed: false,
                });
                match request.confirm_supersede.as_deref() {
                    None => Err("confirmation required".into()),
                    Some(value) if value == required && request.expected_found == found => Ok(()),
                    _ => Err("confirmation or physical tail differs from the preview".into()),
                }
            },
        );
        match (result, preview) {
            (Ok(_), Some(mut response)) => {
                response.completed = true;
                Ok(response)
            }
            (Err(_), Some(response)) if request.confirm_supersede.is_none() => Ok(response),
            (Err(error), _) => Err(error),
            _ => Err(Status::internal("supersede did not produce a preview")),
        }
    })
}

/// Keep the standalone invocation fence alongside the writer's specific fence.
/// Called only when this command failed after dispatching modifying media I/O.
pub(super) fn fence_failed_write(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    status: Status,
) -> Status {
    let mounted = cfg.mounted().expect("mounted failure evidence");
    match remanence_state::fence::raise(
        index,
        &cfg.audit_dir,
        remanence_state::TapeIoFenceInput {
            tape_uuid: mounted.tape_uuid,
            barcode: mounted.barcode.clone(),
            reason: "standalone_write_failed".into(),
            evidence_json: Some(serde_json::json!({"error": status.message()}).to_string()),
        },
        Some(&mounted.endpoint),
        mounted.actor.clone(),
    ) {
        Ok(_) => status,
        Err(error) => super::ActorFailure::preserve(
            &status,
            Status::internal(format!(
                "{}; additionally failed to persist standalone fence: {error}",
                status.message()
            )),
        ),
    }
}
