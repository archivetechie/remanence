//! Idempotent operator entry point for drive-owned closeout. Audit binds the
//! authenticated request; the native ClosedOut record remains media authority.
#[cfg(test)]
use crate::audit_projection::{append_operation_audit, OperationAuditInput};
use crate::{pb, ApiState};
#[cfg(test)]
use ciborium::Value;
use remanence_state::AuditActor;
#[cfg(test)]
use remanence_state::AuditEvent;
#[cfg(test)]
use std::collections::BTreeMap;
use tonic::Status;
use uuid::Uuid;
const KIND: &str = "tape_closeout";
const CLOSEOUT_TAPE_IDEMPOTENCY_CONFLICT: &str =
    "CloseoutTape idempotency key is already bound to a different request";

/// Operator acceptance carries the original fingerprint into a writer-minted
/// operation. A closed close-out is final under its key, as terminal finalize
/// (ruling 25); a retry needs a new key.
pub(crate) fn start(
    state: &ApiState,
    actor: AuditActor,
    request: pb::CloseoutTapeRequest,
) -> Result<pb::OperationRef, Status> {
    use remanence_state::library_log as lib;
    let Some(runtime) = state.catalog.library_runtime() else {
        #[cfg(test)]
        {
            return historical_start(state, actor, request);
        }
        #[cfg(not(test))]
        {
            return Err(Status::failed_precondition(
                "closeout requires shared LibraryAuthority",
            ));
        }
    };
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
    let _start = state
        .operations
        .readiness_start
        .lock()
        .map_err(|_| Status::internal("operation admission lock poisoned"))?;
    let prior = runtime
        .operation_request(
            lib::OperationKind::TapeCloseout,
            &actor_fingerprint,
            *key.as_bytes(),
        )
        .map_err(|e| Status::failed_precondition(e.to_string()))?;
    if let Some((operation, accepted)) = &prior {
        if accepted.request_fingerprint != fingerprint {
            return Err(Status::already_exists(CLOSEOUT_TAPE_IDEMPOTENCY_CONFLICT));
        }
        let id = Uuid::from_bytes(operation.operation);
        let response = pb::OperationRef {
            operation_id: id.as_bytes().to_vec(),
        };
        if state.operations.is_live(&id) {
            return Ok(response);
        }
        match operation.event {
            lib::OperationEvent::Finished(_)=>{
                // This replies to the historical operation only. Later writes
                // or a no-op completion do not authorize or require new work.
                state.operations.register(id,KIND).publish_state(pb::OperationState::Succeeded,&[("phase","complete")]);return Ok(response)
            }
            lib::OperationEvent::Accepted(_)|lib::OperationEvent::Started(_)=>{},
            _=>return Err(Status::failed_precondition("ClosedOperationRetry: this closeout operation has ended; a new operation is required for further physical work"))
        }
    }
    if state
        .readiness_shutdown
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(Status::unavailable(
            "daemon shutdown; maintenance admission closed",
        ));
    }
    let index = state.index()?;
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
    crate::mount::refuse_sealed_tape(&index, tape)?;
    if let Some(pool) = &state.drive_pool {
        pool.check_finalizing(tape)?;
    }
    drop(index);
    let (operation, event) = runtime
        .accept_operation(
            lib::OperationKind::TapeCloseout,
            lib::Accepted {
                actor_fingerprint: actor_fingerprint.as_str().into(),
                idempotency_key: Some(*key.as_bytes()),
                request_fingerprint: fingerprint,
                subject: Some(Uuid::from_bytes(tape).to_string().as_str().into()),
            },
            actor.clone(),
        )
        .map_err(|e| Status::failed_precondition(e.to_string()))?;
    if !matches!(
        event,
        lib::OperationEvent::Accepted(_) | lib::OperationEvent::Started(_)
    ) {
        return Err(Status::failed_precondition(
            "closeout operation closed during acceptance",
        ));
    }
    spawn_closeout(
        state,
        actor,
        request,
        tape,
        key,
        Uuid::from_bytes(operation),
    )
}

#[cfg(test)]
fn historical_start(
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
    let _start = state
        .operations
        .readiness_start
        .lock()
        .map_err(|_| Status::internal("operation admission lock poisoned"))?;
    let mut index = {
        let mut index = state.catalog.write_handle();
        if let Some(pool) = &state.drive_pool {
            index.set_readiness_observer(pool.readiness_wake.clone());
        }
        index
    };
    let existing = index
        .idempotency_scope_record(&actor_fingerprint, KIND, key)
        .map_err(crate::status_from_state_error)?;
    if existing
        .as_ref()
        .is_some_and(|s| s.request_fingerprint != fingerprint)
    {
        return Err(Status::already_exists(CLOSEOUT_TAPE_IDEMPOTENCY_CONFLICT));
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
    if existing.as_ref().and_then(|s| s.terminal_state.as_deref()) == Some("finished") {
        let handle = state.operations.register(id, KIND);
        handle.publish_state(pb::OperationState::Succeeded, &[("phase", "complete")]);
        return Ok(response);
    }
    if state
        .readiness_shutdown
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(Status::unavailable(
            "daemon shutdown; maintenance admission closed",
        ));
    }
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
    crate::mount::refuse_sealed_tape(&index, tape)?;
    if let Some(pool) = &state.drive_pool {
        pool.check_finalizing(tape)?;
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
    spawn_closeout(state, actor, request, tape, key, id)
}

fn spawn_closeout(
    state: &ApiState,
    actor: AuditActor,
    request: pb::CloseoutTapeRequest,
    tape: [u8; 16],
    key: Uuid,
    id: Uuid,
) -> Result<pb::OperationRef, Status> {
    let authority = crate::write_owner::TerminalAuthority::from_api(state)?;
    let handle = state.operations.register(id, KIND);
    let progress = crate::closeout_progress::CloseoutProgress::new(id, tape, Some(handle.clone()))?
        .request(actor.clone(), key);
    progress.phase("queued");
    let state = state.clone();
    tokio::spawn(async move {
        let record = |event, error| {
            progress.record(&mut state.catalog.write_handle(), &authority, event, error)
        };
        let result = async {
            progress.phase("proving");
            crate::mount::closeout_tape(&state, tape, request.expected_pool_id, progress.clone())
                .await
        }
        .await;
        match result {
            Ok(()) => match record(crate::closeout_progress::CloseoutEvent::Finished, None) {
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
                let _ = record(
                    crate::closeout_progress::CloseoutEvent::Failed,
                    Some(&error),
                );
                progress.publish(pb::OperationState::Failed, Some(error.message()));
            }
        }
    });
    Ok(pb::OperationRef {
        operation_id: id.as_bytes().to_vec(),
    })
}
