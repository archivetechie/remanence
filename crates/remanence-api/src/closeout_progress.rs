//! Closeout diagnostics shared by automatic drive work and the operator watch.
//! These observations never replace the native tape log's media authority.
use crate::{operations::OperationHandle, pb};
use ciborium::Value;
use remanence_state::{tape_log::fold::TapeSummary, AuditActor, AuditEvent, CatalogIndex};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};
use tonic::Status;
use uuid::Uuid;

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
    pub(crate) fn detail(&self) -> BTreeMap<String, Value> {
        self.fields()
            .into_iter()
            .map(|(k, v)| (k, Value::Text(v)))
            .collect()
    }
    pub(crate) fn publish(&self, state: pb::OperationState, error: Option<&str>) {
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
