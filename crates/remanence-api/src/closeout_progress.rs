//! Closeout diagnostics shared by automatic drive work and the operator watch.
//! These observations never replace the native tape log's media authority.
use crate::{operations::OperationHandle, pb};
#[cfg(test)]
use ciborium::Value;
use remanence_state::{tape_log::fold::TapeSummary, AuditActor, CatalogIndex};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use tonic::Status;
use uuid::Uuid;

/// Selected by the closeout owner at its actual lifecycle boundary.
#[derive(Clone, Copy)]
pub(crate) enum CloseoutEvent {
    Started,
    Finished,
    Failed,
}
#[derive(Clone)]
pub(crate) struct CloseoutProgress {
    pub(crate) id: Uuid,
    actor: AuditActor,
    key: Option<Uuid>,
    handle: Option<OperationHandle>,
    started: Instant,
    fields: Arc<Mutex<BTreeMap<String, String>>>,
    audited: Arc<Mutex<(bool, bool)>>,
}
impl CloseoutProgress {
    pub(crate) fn new(
        id: Uuid,
        tape: [u8; 16],
        handle: Option<OperationHandle>,
    ) -> Result<Self, Status> {
        Ok(Self {
            id,
            actor: AuditActor::System,
            key: None,
            handle,
            started: Instant::now(),
            audited: Arc::new(Mutex::new((false, false))),
            fields: Arc::new(Mutex::new(BTreeMap::from([
                ("tape_uuid".into(), Uuid::from_bytes(tape).to_string()),
                ("attempt".into(), "1".into()),
                (
                    "started_at_utc".into(),
                    time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)
                        .map_err(|e| Status::internal(e.to_string()))?,
                ),
                ("reserved_bytes".into(), "0".into()),
                ("peak_reserved_bytes".into(), "0".into()),
                ("source".into(), "rebuilt".into()),
            ]))),
        })
    }
    /// Autonomous closeout gets its operation identity from the shared writer.
    /// An inactive planning object publishes no work; its id remains local only.
    pub(crate) fn for_owner(
        authority: &crate::write_owner::TerminalAuthority,
        tape: [u8; 16],
        active: bool,
    ) -> Result<Self, Status> {
        let id = if active {
            if let Some(runtime) = authority.runtime() {
                Uuid::from_bytes(
                    runtime
                        .start_owned_operation(
                            remanence_state::library_log::OperationKind::TapeCloseout,
                            AuditActor::System,
                        )
                        .map_err(|e| Status::failed_precondition(e.to_string()))?,
                )
            } else {
                Uuid::new_v4()
            }
        } else {
            Uuid::new_v4()
        };
        Self::new(id, tape, None)
    }
    /// Publish direct owner-selected lifecycle facts; progress detail stays live
    /// presentation. Historical audit serialization exists only in unit fixtures.
    pub(crate) fn record(
        &self,
        index: &mut CatalogIndex,
        authority: &crate::write_owner::TerminalAuthority,
        event: CloseoutEvent,
        error: Option<&Status>,
    ) -> Result<(), Status> {
        if let Some(runtime) = authority.runtime() {
            use remanence_state::library_log as lib;
            let mut audited = self.audited.lock().unwrap();
            let done = match event {
                CloseoutEvent::Started => &mut audited.0,
                _ => &mut audited.1,
            };
            if *done {
                return Ok(());
            }
            let event = match event {
                CloseoutEvent::Started => lib::OperationEvent::Started(lib::Unit),
                CloseoutEvent::Finished => lib::OperationEvent::Finished(lib::Response {
                    response_fingerprint: None,
                }),
                CloseoutEvent::Failed => lib::OperationEvent::Failed(lib::Failed {
                    retry: None,
                    refusal_code: None,
                    summary: error
                        .ok_or_else(|| {
                            Status::internal("failed closeout requires its actual error")
                        })?
                        .message()
                        .into(),
                }),
            };
            runtime
                .record_operation_event(
                    *self.id.as_bytes(),
                    lib::OperationKind::TapeCloseout,
                    event,
                    self.actor.clone(),
                )
                .map_err(|e| Status::failed_precondition(e.to_string()))?;
            *done = true;
            return Ok(());
        }
        #[cfg(test)]
        {
            let fixture = authority.historical_fixture().expect("historical fixture");
            self.audit(
                index,
                &fixture.audit_dir,
                &fixture.audit_append_lock,
                match event {
                    CloseoutEvent::Started => AuditEvent::OperationStarted,
                    CloseoutEvent::Finished => AuditEvent::OperationFinished,
                    CloseoutEvent::Failed => AuditEvent::OperationFailed,
                },
                error,
            )
        }
        #[cfg(not(test))]
        {
            let _ = index;
            unreachable!("only typed authority exists in production")
        }
    }
    pub(crate) fn request(mut self, actor: AuditActor, key: Uuid) -> Self {
        self.actor = actor;
        self.key = Some(key);
        self
    }
    pub(crate) fn watched(&self) -> bool {
        self.handle.is_some()
    }
    pub(crate) fn bind(&self, summary: &TapeSummary, source: &str) {
        let mut fields = self.fields.lock().unwrap();
        for (key, value) in [
            ("tape_incarnation", summary.incarnation.to_string()),
            (
                "tape_log_sequence",
                summary.last_closed_sequence.to_string(),
            ),
            ("w", summary.protected.to_string()),
            ("t", summary.total.to_string()),
            (
                "unprotected_bytes",
                (u128::from(summary.total - summary.protected) * u128::from(summary.block_size))
                    .to_string(),
            ),
            (
                "already_protected",
                (summary.protected == summary.total).to_string(),
            ),
            ("source", source.to_string()),
        ] {
            fields.insert(key.into(), value);
        }
    }
    pub(crate) fn attempt(&self, attempt: u32) {
        self.fields
            .lock()
            .unwrap()
            .insert("attempt".into(), attempt.to_string());
    }
    pub(crate) fn reserved(&self, bytes: u64) {
        let mut fields = self.fields.lock().unwrap();
        fields.insert("reserved_bytes".into(), bytes.to_string());
        let peak = fields
            .get("peak_reserved_bytes")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
            .max(bytes);
        fields.insert("peak_reserved_bytes".into(), peak.to_string());
    }
    pub(crate) fn phase(&self, phase: &str) {
        self.fields
            .lock()
            .unwrap()
            .insert("phase".into(), phase.into());
        self.publish(
            if phase == "queued" {
                pb::OperationState::Queued
            } else {
                pb::OperationState::Running
            },
            None,
        );
    }
    fn fields(&self) -> BTreeMap<String, String> {
        let mut fields = self.fields.lock().unwrap().clone();
        fields.insert(
            "elapsed_ms".into(),
            self.started.elapsed().as_millis().to_string(),
        );
        fields
    }
    #[cfg(test)]
    pub(crate) fn detail(&self) -> BTreeMap<String, Value> {
        self.fields()
            .into_iter()
            .map(|(k, v)| (k, Value::Text(v)))
            .collect()
    }
    pub(crate) fn publish(&self, state: pb::OperationState, error: Option<&str>) {
        #[cfg(feature = "field-kit")]
        if let Some(path) = std::env::var_os("REM_FIELD_PROGRESS") {
            use std::io::Write;
            if !std::path::PathBuf::from(&path)
                .with_extension("jsonl.stop")
                .exists()
            {
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = writeln!(
                        file,
                        "{}",
                        serde_json::json!({"operation_id":self.id.to_string(), "state":state as i32, "progress":self.fields(), "error":error})
                    );
                    let _ = file.sync_data();
                }
            }
        }
        if let Some(handle) = &self.handle {
            let fields = self.fields();
            let pairs: Vec<_> = fields
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            if let Some(error) = error {
                handle.publish_failed(error, &pairs);
            } else {
                handle.publish_state(state, &pairs);
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn audit(
        &self,
        index: &mut CatalogIndex,
        directory: &Path,
        lock: &Arc<std::sync::Mutex<()>>,
        event: AuditEvent,
        error: Option<&Status>,
    ) -> Result<(), Status> {
        let mut audited = self.audited.lock().unwrap();
        let done = match event {
            AuditEvent::OperationStarted => &mut audited.0,
            AuditEvent::OperationFinished | AuditEvent::OperationFailed => &mut audited.1,
            _ => return Err(Status::internal("invalid closeout audit event")),
        };
        if *done {
            return Ok(());
        }
        let mut detail = self.detail();
        if let Some(error) = error {
            if error.message().starts_with("CloseoutCheckMismatch:") {
                detail.insert("attempt".into(), Value::Text("2".into()));
            }
            detail.insert("reason".into(), Value::Text(error.message().into()));
        }
        crate::audit_projection::append_operation_audit(
            index,
            directory,
            true,
            lock,
            crate::audit_projection::OperationAuditInput {
                actor: self.actor.clone(),
                operation_id: self.id,
                operation_kind: "tape_closeout",
                event,
                subject_kind: "tape",
                subject_id: self.fields().get("tape_uuid").cloned(),
                idempotency_key: self.key,
                detail,
            },
        )?;
        *done = true;
        Ok(())
    }
}

#[cfg(test)]
use remanence_state::AuditEvent;
#[cfg(test)]
use std::path::Path;
