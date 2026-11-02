//! Idempotent operator entry point for drive-owned closeout. Audit binds the
//! authenticated request; the native ClosedOut record remains media authority.
use crate::audit_projection::{append_operation_audit, OperationAuditInput};
use crate::{pb, ApiState};
use ciborium::Value;
use remanence_state::{AuditActor, AuditEvent};
use std::collections::BTreeMap;
use tonic::Status;
use uuid::Uuid;
const KIND: &str = "tape_closeout";

pub(crate) fn start(
    state: &ApiState,
    actor: AuditActor,
    request: pb::CloseoutTapeRequest,
) -> Result<pb::OperationRef, Status> {
    let tape = crate::catalog_conversion::decode_uuid_bytes(&request.tape_uuid, "tape_uuid")?;
    let key = crate::catalog_conversion::decode_required_idempotency(
        request.idempotency_key.as_ref(),
        "CloseoutTape",
    )?;
    if key.is_nil() {
        return Err(Status::invalid_argument(
            "CloseoutTape idempotency_key must not be the nil UUID",
        ));
    }
    if request
        .expected_pool_id
        .as_ref()
        .is_some_and(|p| p.is_empty() || p.trim() != p)
    {
        return Err(Status::invalid_argument(
            "expected_pool_id must be non-empty canonical text without surrounding whitespace",
        ));
    }
    let actor_fingerprint = crate::catalog_conversion::audit_actor_fingerprint(&actor);
    let fingerprint = crate::catalog_conversion::manual_finalize_request_fingerprint(
        tape,
        request.expected_pool_id.as_deref(),
        &actor_fingerprint,
        KIND.as_bytes(),
    );
    if let Some(pool) = &state.drive_pool {
        pool.check_finalizing(tape)?;
    }
    let _start = state
        .operations
        .readiness_start
        .lock()
        .map_err(|_| Status::internal("operation admission lock poisoned"))?;
    let mut index = state.index_write()?;
    let row = index
        .get_tape(&tape)
        .map_err(crate::status_from_state_error)?
        .ok_or_else(|| Status::not_found("tape not found"))?;
    if request
        .expected_pool_id
        .as_deref()
        .is_some_and(|p| row.pool_id.as_deref() != Some(p))
    {
        return Err(Status::failed_precondition(
            "CloseoutTape expected_pool_id does not match the tape's assigned pool",
        ));
    }
    let existing = index
        .idempotency_scope_record(&actor_fingerprint, KIND, key)
        .map_err(crate::status_from_state_error)?;
    if existing
        .as_ref()
        .is_some_and(|s| s.request_fingerprint != fingerprint)
    {
        return Err(Status::already_exists(
            "CloseoutTape idempotency key is already bound to a different request",
        ));
    }
    let id = existing
        .as_ref()
        .map_or_else(Uuid::new_v4, |s| s.operation_id);
    let response = pb::OperationRef {
        operation_id: id.as_bytes().to_vec(),
    };
    if state.operations.is_live(&id)
        || state
            .operations
            .latest(&id)
            .is_some_and(|s| s.state == pb::OperationState::Succeeded as i32)
    {
        return Ok(response);
    }
    if existing.is_none() {
        append_operation_audit(
            &mut index,
            state.audit_dir.as_ref(),
            true,
            &state.audit_append_lock,
            OperationAuditInput {
                actor: actor.clone(),
                operation_id: id,
                operation_kind: KIND,
                event: AuditEvent::RequestReceived,
                subject_kind: "tape",
                subject_id: Some(Uuid::from_bytes(tape).to_string()),
                idempotency_key: Some(key),
                detail: BTreeMap::from([
                    ("tape_uuid".into(), Value::Bytes(tape.to_vec())),
                    ("actor_fingerprint".into(), Value::Text(actor_fingerprint)),
                    (
                        "request_fingerprint".into(),
                        Value::Bytes(fingerprint.to_vec()),
                    ),
                    (
                        "expected_pool_id".into(),
                        request
                            .expected_pool_id
                            .clone()
                            .map_or(Value::Null, Value::Text),
                    ),
                ]),
            },
        )?;
    }
    let handle = state.operations.register(id, KIND);
    if existing.as_ref().and_then(|s| s.terminal_state.as_deref()) == Some("finished") {
        handle.publish_state(pb::OperationState::Succeeded, &[("phase", "complete")]);
        return Ok(response);
    }
    let progress = crate::closeout_progress::CloseoutProgress::new(id, tape, Some(handle.clone()))?
        .request(actor.clone(), key);
    progress.phase("queued");
    let state = state.clone();
    tokio::spawn(async move {
        let record = |event, detail| -> Result<(), Status> {
            append_operation_audit(
                &mut state.index_write()?,
                state.audit_dir.as_ref(),
                true,
                &state.audit_append_lock,
                OperationAuditInput {
                    actor: actor.clone(),
                    operation_id: id,
                    operation_kind: KIND,
                    event,
                    subject_kind: "tape",
                    subject_id: Some(Uuid::from_bytes(tape).to_string()),
                    idempotency_key: Some(key),
                    detail,
                },
            )
        };
        let result = async {
            progress.phase("proving");
            crate::mount::closeout_tape(&state, tape, request.expected_pool_id, progress.clone())
                .await
        }
        .await;
        match result {
            Ok(()) => match record(AuditEvent::OperationFinished, progress.detail()) {
                Ok(()) => {
                    progress.phase("complete");
                    progress.publish(pb::OperationState::Succeeded, None);
                }
                Err(error) => handle.publish_failed(error.message(), &[("phase", "projecting")]),
            },
            Err(error) => {
                if error.message().starts_with("CloseoutCheckMismatch:") {
                    progress.attempt(2);
                }
                let _ = record(AuditEvent::OperationFailed, {
                    let mut detail = progress.detail();
                    detail.insert("reason".into(), Value::Text(error.message().into()));
                    detail.insert("error_summary".into(), Value::Text(error.to_string()));
                    detail
                });
                progress.publish(pb::OperationState::Failed, Some(error.message()));
            }
        }
    });
    Ok(response)
}
