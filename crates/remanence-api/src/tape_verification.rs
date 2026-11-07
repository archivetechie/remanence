//! Full tape verification lifecycle, retained verdicts and bounded progress.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ciborium::value::Value;
use remanence_library::verification_io::VerificationPhase;
use remanence_state::library_log as lib;
use remanence_state::library_runtime::VerdictFacts;
use remanence_state::AuditActor;
#[cfg(test)]
use remanence_state::AuditEvent;
use tokio::sync::oneshot;
use tonic::Status;
use uuid::Uuid;

#[cfg(test)]
use crate::audit_projection::{append_operation_audit, OperationAuditInput};
use crate::operations::OperationHandle;
use crate::write_owner::terminal_inventory::{VerdictEvidence, VerifiedTapeIndex};
use crate::{pb, ApiState};

const KIND: &str = "tape_verification";
type Verdict = Result<pb::TapeIndexVerification, Status>;
type Verified = Result<VerifiedTapeIndex, Status>;

/// Start independently of the request lifetime. The receiver preserves unary errors.
pub(crate) fn start(
    state: &ApiState,
    actor: AuditActor,
    tape_uuid: [u8; 16],
    ready_wait: Duration,
) -> Result<(pb::OperationRef, oneshot::Receiver<Verdict>), Status> {
    let fingerprint: [u8; 32] = {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"tape_verification\0");
        hasher.update(tape_uuid);
        hasher.finalize().into()
    };
    let id = state.accept_owner_request(
        actor,
        KIND,
        "tape",
        Uuid::from_bytes(tape_uuid).to_string(),
        fingerprint,
    )?;
    let handle = state.operations.register(id, KIND);
    let progress = Progress::new(handle.clone(), tape_uuid);
    progress.route(remanence_parity::VerificationRouteStatus {
        route: state.verification_route,
        fell_back: false,
        fallback_reason: None,
    });
    let state = state.clone();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let result: Verified = async {
            record_started(&handle, || {
                record(
                    &state,
                    &handle,
                    tape_uuid,
                    lib::OperationEvent::Started(lib::Unit),
                    BTreeMap::new(),
                )
            })?;
            check_cancelled(&handle)?;
            let mut deadline = None;
            loop {
                match crate::mount::verify_tape_index(
                    &state,
                    tape_uuid,
                    handle.clone(),
                    progress.clone(),
                )
                .await
                {
                    Ok(verdict) => break Ok(verdict),
                    Err(error) => {
                        crate::readiness_wait::wait_after_refusal(
                            &state,
                            &handle,
                            error,
                            ready_wait,
                            &mut deadline,
                        )
                        .await?
                    }
                }
            }
        }
        .await;
        // Mount ownership is released before a terminal status becomes visible.
        // Honour a cancellation that arrives while the mount path releases ownership.
        // The typed verdict is recorded before `Finished`; if that record fails the
        // operation takes the failure path below and `Finished` is never appended.
        let result: Verdict = result.and_then(|(verdict, evidence)| {
            check_cancelled(&handle)?;
            // The tape is released by now. A test injects its write here, in the window.
            #[cfg(test)]
            if let Some(hook) = state
                .drive_pool
                .as_ref()
                .and_then(|pool| pool.verdict_window_hook.lock().unwrap().take())
            {
                hook();
            }
            record_verdict(&state, &handle, tape_uuid, &verdict, evidence)?;
            Ok(verdict)
        });
        let result = match result {
            Ok(verdict) => {
                // The test-only legacy arm still reads its verdict back from the audit detail;
                // the typed arm recorded the verdict in `record_verdict` above.
                #[cfg(test)]
                let detail = legacy_verdict_detail(&verdict);
                #[cfg(not(test))]
                let detail = BTreeMap::new();
                match record(
                    &state,
                    &handle,
                    tape_uuid,
                    lib::OperationEvent::Finished(lib::Response {
                        response_fingerprint: None,
                    }),
                    detail,
                ) {
                    Ok(()) => {
                        handle.complete_verification(verdict.clone());
                        Ok(verdict)
                    }
                    Err(error) => {
                        handle.publish_failed(error.message(), &[]);
                        Err(error)
                    }
                }
            }
            Err(error) => {
                let cancelled = error.code() == tonic::Code::Cancelled;
                let event = if cancelled {
                    lib::OperationEvent::CancelledBeforeDispatch(lib::Unit)
                } else {
                    lib::OperationEvent::Failed(lib::Failed {
                        retry: None,
                        refusal_code: None,
                        summary: error.message().into(),
                    })
                };
                #[cfg(test)]
                let detail =
                    BTreeMap::from([("error_summary".into(), Value::Text(error.message().into()))]);
                #[cfg(not(test))]
                let detail = BTreeMap::new();
                if let Err(audit_error) = record(&state, &handle, tape_uuid, event, detail) {
                    tracing::error!(%audit_error, "record verification termination");
                }
                if cancelled {
                    handle.publish_state(pb::OperationState::Cancelled, &[]);
                } else {
                    handle.publish_failed(error.message(), &[]);
                }
                Err(error)
            }
        };
        let _ = tx.send(result);
    });
    Ok((
        pb::OperationRef {
            operation_id: id.as_bytes().to_vec(),
        },
        rx,
    ))
}

/// Verdict fields of the legacy audit record, read back by the legacy projection.
#[cfg(test)]
fn legacy_verdict_detail(verdict: &pb::TapeIndexVerification) -> BTreeMap<String, Value> {
    let mut detail = BTreeMap::from([(
        "verification_state".into(),
        Value::Integer(verdict.state.into()),
    )]);
    if let Some(digest) = &verdict.edition_digest {
        detail.insert("edition_digest".into(), Value::Bytes(digest.clone()));
    }
    detail
}

/// The verdict outcome of a proto verification state: states 2, 3 and 4 in order. State 1,
/// the deferred legacy response, and any other state record no verdict.
pub(crate) fn verdict_outcome(state: i32) -> Option<lib::VerificationOutcome> {
    use pb::TapeIndexVerificationState as State;
    match State::try_from(state) {
        Ok(State::VerifiedComplete) => Some(lib::VerificationOutcome::VerifiedComplete),
        Ok(State::VerifiedDegraded) => Some(lib::VerificationOutcome::VerifiedDegraded),
        Ok(State::RecoveryRequired) => Some(lib::VerificationOutcome::RecoveryRequired),
        _ => None,
    }
}

/// Record the typed `VerificationVerdict` of a finished verification (ruling 34). Proto
/// states 2, 3 and 4 are the three outcomes; state 1, the deferred legacy response, and any
/// other state record no verdict. The runtime resolves `at` and the tape identity under its
/// dispatch lease; `evidence` carries the checks that ran and the unprotected tail, read from
/// the verifier's own outcome. The edition digest is the verified terminal edition's.
fn record_verdict(
    state: &ApiState,
    handle: &OperationHandle,
    tape_uuid: [u8; 16],
    verdict: &pb::TapeIndexVerification,
    evidence: VerdictEvidence,
) -> Result<(), Status> {
    let Some(outcome) = verdict_outcome(verdict.state) else {
        return Ok(());
    };
    let edition_digest = verdict
        .edition_digest
        .as_deref()
        .map(|digest| {
            <[u8; 32]>::try_from(digest)
                .map_err(|_| Status::internal("verification edition digest is not 32 bytes"))
        })
        .transpose()?;
    let Some(runtime) = state.catalog.library_runtime() else {
        // Only the test-only legacy arm runs without a runtime: production fails at `Started`.
        #[cfg(test)]
        return Ok(());
        #[cfg(not(test))]
        return Err(Status::failed_precondition(
            "tape verification requires shared LibraryAuthority",
        ));
    };
    runtime
        .record_verification_verdict(
            *handle.op_id_uuid().as_bytes(),
            tape_uuid,
            VerdictFacts {
                native_reference: evidence.native_reference,
                outcome,
                unprotected: evidence.unprotected,
                modes: evidence.modes,
                edition_digest,
            },
            AuditActor::System,
        )
        .map(|_| ())
        .map_err(|e| Status::failed_precondition(e.to_string()))
}

/// Append one lifecycle event. With a library runtime the typed `event` is the
/// record; `legacy_detail` feeds only the test-only legacy audit arm.
fn record(
    state: &ApiState,
    handle: &OperationHandle,
    tape_uuid: [u8; 16],
    event: lib::OperationEvent,
    legacy_detail: BTreeMap<String, Value>,
) -> Result<(), Status> {
    if let Some(runtime) = state.catalog.library_runtime() {
        return runtime
            .record_operation_event(
                *handle.op_id_uuid().as_bytes(),
                lib::OperationKind::TapeVerification,
                event,
                AuditActor::System,
            )
            .map_err(|e| Status::failed_precondition(e.to_string()));
    }
    #[cfg(test)]
    {
        let event = match event {
            lib::OperationEvent::Started(_) => AuditEvent::OperationStarted,
            lib::OperationEvent::Finished(_) => AuditEvent::OperationFinished,
            lib::OperationEvent::Failed(_) => AuditEvent::OperationFailed,
            lib::OperationEvent::CancelledBeforeDispatch(_) => AuditEvent::CancelledBeforeDispatch,
            other => {
                return Err(Status::internal(format!(
                    "tape verification does not record {other:?}"
                )))
            }
        };
        let mut detail = legacy_detail;
        detail.insert("tape_uuid".into(), Value::Bytes(tape_uuid.to_vec()));
        append_operation_audit(
            &mut {
                let mut index = state.catalog.write_handle();
                if let Some(pool) = &state.drive_pool {
                    index.set_readiness_observer(pool.readiness_wake.clone());
                }
                index
            },
            state.audit_dir.as_ref(),
            state.audit_fsync,
            &state.audit_append_lock,
            OperationAuditInput {
                actor: AuditActor::System,
                operation_id: handle.op_id_uuid(),
                operation_kind: KIND,
                event,
                subject_kind: "tape",
                subject_id: Some(Uuid::from_bytes(tape_uuid).to_string()),
                idempotency_key: None,
                detail,
            },
        )
    }
    #[cfg(not(test))]
    {
        let _ = (tape_uuid, legacy_detail);
        Err(Status::failed_precondition(
            "tape verification requires shared LibraryAuthority",
        ))
    }
}

/// Run the step that records `Started`. There are three outcomes: `Ok(())` when it was
/// recorded; an error with code `Cancelled` when the handle is cancelled, which the caller
/// returns to take the cancelled branch; and the record error, unchanged, when it is not.
///
/// A cancel can land after `start` returns but before the task runs. The typed log then
/// holds Accepted, CancelRequested and refuses Started, so the check comes first and
/// `Started` is not recorded. A cancel can still land between that check and the record, so
/// a failed record is checked again before its error is returned.
fn record_started(
    handle: &OperationHandle,
    append: impl FnOnce() -> Result<(), Status>,
) -> Result<(), Status> {
    check_cancelled(handle)?;
    if let Err(error) = append() {
        check_cancelled(handle)?;
        return Err(error);
    }
    Ok(())
}

pub(crate) fn check_cancelled(handle: &OperationHandle) -> Result<(), Status> {
    if handle.is_cancelled() {
        Err(Status::cancelled("tape verification cancelled"))
    } else {
        Ok(())
    }
}

/// Rate-limit ordinary updates; entering or leaving a phase always publishes.
#[derive(Clone)]
pub(crate) struct Progress(Arc<Mutex<ProgressState>>);
struct ProgressState {
    handle: OperationHandle,
    tape_uuid: String,
    drive_serial: String,
    bay: String,
    started: Instant,
    last: Option<(VerificationPhase, Instant)>,
    latest: (VerificationPhase, u64, Option<u64>),
    route: remanence_parity::VerificationRouteStatus,
}
impl Progress {
    pub(crate) fn new(handle: OperationHandle, tape_uuid: [u8; 16]) -> Self {
        Self(Arc::new(Mutex::new(ProgressState {
            handle,
            tape_uuid: Uuid::from_bytes(tape_uuid).to_string(),
            drive_serial: "unknown".into(),
            bay: "unknown".into(),
            started: Instant::now(),
            last: None,
            latest: (VerificationPhase::Setup, 0, None),
            route: remanence_parity::VerificationRouteStatus {
                route: remanence_parity::VerificationRoute::Walk,
                fell_back: false,
                fallback_reason: None,
            },
        })))
    }
    pub(crate) fn route(&self, route: remanence_parity::VerificationRouteStatus) {
        let mut state = self.0.lock().unwrap();
        state.route = route;
        state.last = None;
        let (phase, bytes, file) = state.latest;
        drop(state);
        self.publish(phase, bytes, file);
    }
    pub(crate) fn mounted(&self, serial: Option<&str>, bay: u16) {
        let mut state = self.0.lock().unwrap();
        state.drive_serial = serial.unwrap_or("unknown").into();
        state.bay = bay.to_string();
    }
    pub(crate) fn publish(&self, phase: VerificationPhase, bytes: u64, tape_file: Option<u64>) {
        let mut state = self.0.lock().unwrap();
        let now = Instant::now();
        state.latest = (phase, bytes, tape_file);
        if state.last.is_some_and(|(previous, at)| {
            previous == phase && now.duration_since(at) < Duration::from_secs(5)
        }) {
            return;
        }
        state.last = Some((phase, now));
        state.handle.publish_state(
            pb::OperationState::Running,
            &[
                ("tape_uuid", &state.tape_uuid),
                ("drive_serial", &state.drive_serial),
                ("bay", &state.bay),
                ("phase", phase.name()),
                ("route", state.route.route.name()),
                ("fallback_reason", state.route.fallback_reason.unwrap_or("")),
                (
                    "one_pass_fell_back",
                    if state.route.fell_back {
                        "true"
                    } else {
                        "false"
                    },
                ),
                (
                    "tape_file",
                    &tape_file
                        .map(|file| file.to_string())
                        .unwrap_or_else(|| "unknown".into()),
                ),
                ("bytes_read", &bytes.to_string()),
                (
                    "elapsed_ms",
                    &state.started.elapsed().as_millis().to_string(),
                ),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Started` is appended once, and nothing else happens, when no cancel is pending.
    #[test]
    fn record_started_returns_ok_when_the_record_succeeds() {
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), KIND);
        let mut calls = 0;
        let outcome = record_started(&handle, || {
            calls += 1;
            Ok(())
        });
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(calls, 1);
    }

    /// A cancel that lands between the first check and the record turns the record's
    /// refusal into a cancellation, not a failure. The closure plays that cancel: it sets
    /// the flag, as `Daemon::cancel_operation` does, and then reports the refusal the typed
    /// log gives `Started` after `CancelRequested`.
    #[test]
    fn record_started_cancelled_during_the_record_is_cancelled_not_failed() {
        let registry = crate::operations::OperationRegistry::default();
        let id = Uuid::new_v4();
        let handle = registry.register(id, KIND);
        let outcome = record_started(&handle, || {
            registry.request_cancel(&id).unwrap();
            Err(Status::failed_precondition(
                "Started follows nothing or Accepted",
            ))
        });
        let error = outcome.expect_err("the record was refused");
        assert_eq!(error.code(), tonic::Code::Cancelled, "{error:?}");
        assert_eq!(error.message(), "tape verification cancelled");
    }

    /// A refused record with no cancel pending is a failure, returned unchanged.
    #[test]
    fn record_started_returns_the_record_error_when_not_cancelled() {
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(Uuid::new_v4(), KIND);
        let outcome = record_started(&handle, || Err(Status::internal("store unavailable")));
        let error = outcome.expect_err("the record was refused");
        assert_eq!(error.code(), tonic::Code::Internal, "{error:?}");
        assert_eq!(error.message(), "store unavailable");
    }

    /// Route changes bypass throttling and persist with ordinary phase progress.
    #[tokio::test]
    async fn verification_progress_records_route_and_fallback() {
        use remanence_parity::{VerificationRoute, VerificationRouteStatus};
        use tokio_stream::StreamExt;
        let registry = crate::operations::OperationRegistry::default();
        let id = Uuid::new_v4();
        let handle = registry.register(id, KIND);
        let progress = Progress::new(handle.clone(), [1; 16]);
        progress.route(VerificationRouteStatus {
            route: VerificationRoute::OnePass,
            fell_back: false,
            fallback_reason: None,
        });
        progress.publish(VerificationPhase::Setup, 100, Some(1)); // throttled, but retained
        progress.route(VerificationRouteStatus {
            route: VerificationRoute::Walk,
            fell_back: true,
            fallback_reason: Some("tape_io"),
        });
        progress.publish(VerificationPhase::ProtectedData, 123, Some(2));
        handle.publish_state(pb::OperationState::Cancelled, &[]);
        let events = registry.watch(&id).unwrap().collect::<Vec<_>>().await;
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].as_ref().unwrap().progress["route"], "one_pass");
        assert_eq!(
            events[0].as_ref().unwrap().progress["one_pass_fell_back"],
            "false"
        );
        assert_eq!(events[1].as_ref().unwrap().progress["bytes_read"], "100");
        assert_eq!(events[1].as_ref().unwrap().progress["tape_file"], "1");
        for event in &events[1..3] {
            assert_eq!(event.as_ref().unwrap().progress["route"], "walk");
            assert_eq!(
                event.as_ref().unwrap().progress["fallback_reason"],
                "tape_io"
            );
            assert_eq!(
                event.as_ref().unwrap().progress["one_pass_fell_back"],
                "true"
            );
        }
    }

    /// Repeated scoped work restores accounting without publishing a false parent phase.
    #[tokio::test]
    async fn repeated_scoped_phases_respect_progress_rate() {
        use tokio_stream::StreamExt;
        let registry = crate::operations::OperationRegistry::default();
        let id = Uuid::new_v4();
        let handle = registry.register(id, KIND);
        let progress = Progress::new(handle.clone(), [1; 16]);
        let context = remanence_library::verification_io::VerificationContext::default();
        let _control = context.control(
            handle.cancel_flag(),
            Arc::new(move |phase, bytes, file| progress.publish(phase, bytes, file)),
        );
        for phase in [
            VerificationPhase::ProtectedData,
            VerificationPhase::FallbackData,
        ] {
            for _ in 0..100 {
                let _scope = context.phase(phase);
                context.check_cancelled().unwrap();
            }
        }
        handle.publish_state(pb::OperationState::Cancelled, &[]);
        let events = registry.watch(&id).unwrap().collect::<Vec<_>>().await;
        assert_eq!(events.len(), 4); // setup, protected, fallback, terminal
    }

    /// Phase transitions bypass the five-second throttle; ordinary updates do not.
    #[tokio::test]
    async fn progress_keys_and_throttle() {
        use tokio_stream::StreamExt;
        let registry = crate::operations::OperationRegistry::default();
        let id = Uuid::new_v4();
        let handle = registry.register(id, KIND);
        let progress = Progress::new(handle.clone(), [1; 16]);
        progress.mounted(Some("DRIVE001"), 256);
        progress.publish(VerificationPhase::Setup, 0, None);
        progress.publish(VerificationPhase::Setup, 100, None);
        progress.publish(VerificationPhase::Walk, 100, None);
        progress.publish(VerificationPhase::Walk, 200, None);
        progress.0.lock().unwrap().last.as_mut().unwrap().1 -= Duration::from_secs(5);
        progress.publish(VerificationPhase::Walk, 300, None);
        handle.publish_state(pb::OperationState::Cancelled, &[]);
        let events = registry.watch(&id).unwrap().collect::<Vec<_>>().await;
        assert_eq!(events.len(), 4);
        for event in &events[..3] {
            let event = event.as_ref().unwrap();
            for key in [
                "tape_uuid",
                "drive_serial",
                "bay",
                "phase",
                "route",
                "one_pass_fell_back",
                "fallback_reason",
                "tape_file",
                "bytes_read",
                "elapsed_ms",
            ] {
                assert!(event.progress.contains_key(key), "{key}");
            }
            assert_eq!(event.progress["drive_serial"], "DRIVE001");
            assert_eq!(event.progress["bay"], "256");
        }
        assert_eq!(events[2].as_ref().unwrap().progress["bytes_read"], "300");
    }
}
