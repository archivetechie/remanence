//! Full tape verification lifecycle, retained verdicts and bounded progress.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ciborium::value::Value;
use remanence_library::verification_io::VerificationPhase;
use remanence_state::{AuditActor, AuditEvent};
use tokio::sync::oneshot;
use tonic::Status;
use uuid::Uuid;

use crate::audit_projection::{append_operation_audit, OperationAuditInput};
use crate::operations::OperationHandle;
use crate::{pb, ApiState};

const KIND: &str = "tape_verification";
type Verdict = Result<pb::TapeIndexVerification, Status>;

/// Start independently of the request lifetime. The receiver preserves unary errors.
pub(crate) fn start(
    state: &ApiState,
    actor: AuditActor,
    tape_uuid: [u8; 16],
) -> Result<(pb::OperationRef, oneshot::Receiver<Verdict>), Status> {
    let id = Uuid::new_v4();
    state.record_request_received(actor, id, KIND, &tape_uuid, None)?;
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
        let result = async {
            record(
                &state,
                &handle,
                tape_uuid,
                AuditEvent::OperationStarted,
                BTreeMap::new(),
            )?;
            check_cancelled(&handle)?;
            crate::mount::verify_tape_index(&state, tape_uuid, handle.clone(), progress).await
        }
        .await;
        // Mount ownership is released before a terminal status becomes visible.
        // Honour a cancellation that arrives while the mount path releases ownership.
        let result = result.and_then(|verdict| {
            check_cancelled(&handle)?;
            Ok(verdict)
        });
        let result = match result {
            Ok(verdict) => {
                let mut detail = BTreeMap::from([(
                    "verification_state".into(),
                    Value::Integer(verdict.state.into()),
                )]);
                if let Some(digest) = &verdict.edition_digest {
                    detail.insert("edition_digest".into(), Value::Bytes(digest.clone()));
                }
                match record(
                    &state,
                    &handle,
                    tape_uuid,
                    AuditEvent::OperationFinished,
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
                    AuditEvent::CancelledBeforeDispatch
                } else {
                    AuditEvent::OperationFailed
                };
                let detail =
                    BTreeMap::from([("error_summary".into(), Value::Text(error.message().into()))]);
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

fn record(
    state: &ApiState,
    handle: &OperationHandle,
    tape_uuid: [u8; 16],
    event: AuditEvent,
    mut detail: BTreeMap<String, Value>,
) -> Result<(), Status> {
    detail.insert("tape_uuid".into(), Value::Bytes(tape_uuid.to_vec()));
    append_operation_audit(
        &mut state.index_write()?,
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
