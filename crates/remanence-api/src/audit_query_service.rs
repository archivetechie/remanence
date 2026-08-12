//! Read-only audit query service over the authoritative hash chain.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::pin::Pin;

use remanence_state::{AuditActor, AuditEvent, AuditRecord, FileAuditLog, SourceLayer};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::auth::{authorize_request, AuthPermission};
use crate::catalog_conversion::{send_stream_item, timestamp_from_rfc3339};
use crate::pb;
use crate::startup_media_readiness::status_from_state_error;

const AUDIT_STREAM_BUFFER: usize = 32;
type AuditEntryStream =
    Pin<Box<dyn Stream<Item = Result<pb::AuditEntry, Status>> + Send + 'static>>;

/// Read-only Layer 5 audit-query service over the authoritative hash chain.
#[derive(Clone)]
pub struct AuditApi {
    pub(crate) state: ApiState,
}

#[tonic::async_trait]
impl pb::audit_server::Audit for AuditApi {
    type QueryAuditStream = AuditEntryStream;

    async fn query_audit(
        &self,
        request: Request<pb::QueryAuditRequest>,
    ) -> Result<Response<Self::QueryAuditStream>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let query = AuditQuery::try_from(request.into_inner())?;
        Ok(Response::new(audit_entry_stream(
            self.state.audit_dir.as_ref().clone(),
            query,
        )))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct AuditQuery {
    since: Option<OffsetDateTime>,
    until: Option<OffsetDateTime>,
    filter: BTreeMap<String, String>,
}

impl TryFrom<pb::QueryAuditRequest> for AuditQuery {
    type Error = Status;

    fn try_from(request: pb::QueryAuditRequest) -> Result<Self, Self::Error> {
        let since = request
            .since
            .as_ref()
            .map(|timestamp| audit_query_timestamp(timestamp, "since"))
            .transpose()?;
        let until = request
            .until
            .as_ref()
            .map(|timestamp| audit_query_timestamp(timestamp, "until"))
            .transpose()?;
        if since
            .zip(until)
            .is_some_and(|(since, until)| since >= until)
        {
            return Err(Status::invalid_argument(
                "audit query requires since to be earlier than until",
            ));
        }
        let mut filter = BTreeMap::new();
        for (raw_key, raw_value) in request.filter {
            let key = raw_key.trim().to_ascii_lowercase();
            let mut value = raw_value.trim().to_string();
            if value.is_empty() {
                return Err(Status::invalid_argument(format!(
                    "audit filter {raw_key:?} must not be empty"
                )));
            }
            match key.as_str() {
                "session_id" | "operation_id" => {
                    value = Uuid::parse_str(value.as_str())
                        .map_err(|_| {
                            Status::invalid_argument(format!("audit filter {key} must be a UUID"))
                        })?
                        .to_string();
                }
                "subject_id" => {
                    if let Ok(subject_uuid) = Uuid::parse_str(value.as_str()) {
                        value = subject_uuid.to_string();
                    }
                }
                "event_kind" | "event" | "kind" | "actor" | "source_layer" | "subject_kind" => {}
                _ => {
                    return Err(Status::invalid_argument(format!(
                        "unsupported audit filter {raw_key:?}"
                    )))
                }
            }
            filter.insert(key, value);
        }
        Ok(Self {
            since,
            until,
            filter,
        })
    }
}

pub(crate) fn audit_query_timestamp(
    timestamp: &prost_types::Timestamp,
    field: &str,
) -> Result<OffsetDateTime, Status> {
    if !(0..1_000_000_000).contains(&timestamp.nanos) {
        return Err(Status::invalid_argument(format!(
            "{field}.nanos must be in 0..1000000000"
        )));
    }
    OffsetDateTime::from_unix_timestamp(timestamp.seconds)
        .ok()
        .and_then(|base| base.checked_add(time::Duration::nanoseconds(i64::from(timestamp.nanos))))
        .ok_or_else(|| Status::invalid_argument(format!("{field} is outside the supported range")))
}

pub(crate) fn audit_entry_stream(audit_dir: PathBuf, query: AuditQuery) -> AuditEntryStream {
    let (tx, rx) = tokio::sync::mpsc::channel(AUDIT_STREAM_BUFFER);
    tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<(), Status> {
            let mut match_error = None;
            FileAuditLog::replay_incremental(&audit_dir, |record| {
                let matched = match audit_record_matches(&record, &query) {
                    Ok(matched) => matched,
                    Err(status) => {
                        match_error = Some(status);
                        return ControlFlow::Break(());
                    }
                };
                if !matched {
                    return ControlFlow::Continue(());
                }
                send_stream_item(&tx, audit_record_to_proto(record))
            })
            .map_err(status_from_state_error)?;
            if let Some(status) = match_error {
                return Err(status);
            }
            Ok(())
        })();
        if let Err(status) = result {
            let _ = tx.blocking_send(Err(status));
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

pub(crate) fn audit_record_matches(
    record: &AuditRecord,
    query: &AuditQuery,
) -> Result<bool, Status> {
    let timestamp = OffsetDateTime::parse(record.timestamp_utc.as_str(), &Rfc3339)
        .map_err(|err| Status::internal(format!("stored audit timestamp is invalid: {err}")))?;
    if query.since.is_some_and(|since| timestamp < since)
        || query.until.is_some_and(|until| timestamp >= until)
    {
        return Ok(false);
    }
    for (key, expected) in &query.filter {
        let matched = match key.as_str() {
            "session_id" => record
                .session_id
                .is_some_and(|value| value.to_string() == *expected),
            "operation_id" => record
                .operation_id
                .is_some_and(|value| value.to_string() == *expected),
            "event_kind" | "event" | "kind" => audit_event_name(&record.event) == expected,
            "actor" => audit_actor_name(&record.actor) == *expected,
            "source_layer" => audit_source_layer_name(&record.source_layer) == expected,
            "subject_kind" => record.subject.kind == *expected,
            "subject_id" => audit_subject_id_matches(record.subject.id.as_deref(), expected),
            _ => unreachable!("audit filter keys are validated before streaming"),
        };
        if !matched {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn audit_subject_id_matches(actual: Option<&str>, expected: &str) -> bool {
    actual.is_some_and(|actual| {
        actual == expected
            || Uuid::parse_str(actual).is_ok_and(|actual_uuid| {
                Uuid::parse_str(expected).is_ok_and(|expected_uuid| actual_uuid == expected_uuid)
            })
    })
}

pub(crate) fn audit_record_to_proto(record: AuditRecord) -> Result<pb::AuditEntry, Status> {
    let timestamp = timestamp_from_rfc3339(record.timestamp_utc.as_str())
        .ok_or_else(|| Status::internal("stored audit timestamp is invalid"))?;
    let detail_json = serde_json::to_string(&record.detail)
        .map_err(|err| Status::internal(format!("serialize audit detail as JSON: {err}")))?;
    Ok(pb::AuditEntry {
        sequence: record.sequence,
        timestamp: Some(timestamp),
        actor: audit_actor_name(&record.actor),
        source_layer: audit_source_layer_name(&record.source_layer).to_string(),
        operation_id: record
            .operation_id
            .map(|value| value.as_bytes().to_vec())
            .unwrap_or_default(),
        session_id: record
            .session_id
            .map(|value| value.as_bytes().to_vec())
            .unwrap_or_default(),
        event_kind: audit_event_name(&record.event).to_string(),
        detail_json,
        software_build: record.software_build,
    })
}

pub(crate) fn audit_actor_name(actor: &AuditActor) -> String {
    match actor {
        AuditActor::System => "system".to_string(),
        AuditActor::User(id) => format!("user:{id}"),
        AuditActor::Service(id) => format!("service:{id}"),
    }
}

pub(crate) fn audit_source_layer_name(source: &SourceLayer) -> &'static str {
    match source {
        SourceLayer::Layer2 => "layer2",
        SourceLayer::Layer3b => "layer3b",
        SourceLayer::Layer3c => "layer3c",
        SourceLayer::Layer4 => "layer4",
        SourceLayer::Layer5 => "layer5",
    }
}

pub(crate) fn audit_event_name(event: &AuditEvent) -> &'static str {
    match event {
        AuditEvent::RequestReceived => "RequestReceived",
        AuditEvent::OperationStarted => "OperationStarted",
        AuditEvent::OperationProgress => "OperationProgress",
        AuditEvent::OperationFinished => "OperationFinished",
        AuditEvent::OperationFailed => "OperationFailed",
        AuditEvent::CancelRequested => "CancelRequested",
        AuditEvent::CancelledBeforeDispatch => "CancelledBeforeDispatch",
        AuditEvent::CompletedAfterCancel => "CompletedAfterCancel",
        AuditEvent::CancellationRejected => "CancellationRejected",
        AuditEvent::CompletionUnknown => "CompletionUnknown",
        AuditEvent::SessionOpened => "SessionOpened",
        AuditEvent::SessionCheckpointed => "SessionCheckpointed",
        AuditEvent::SessionClosed => "SessionClosed",
        AuditEvent::SessionOrphaned => "SessionOrphaned",
        AuditEvent::SessionLostByRestart => "SessionLostByRestart",
        AuditEvent::ClockRegressionObserved => "ClockRegressionObserved",
        AuditEvent::ClockForwardJumpObserved => "ClockForwardJumpObserved",
        AuditEvent::HardwareWarning => "HardwareWarning",
        AuditEvent::RecoveryEvent => "RecoveryEvent",
        AuditEvent::ConfigLoaded => "ConfigLoaded",
        AuditEvent::ConfigRejected => "ConfigRejected",
        AuditEvent::IndexRebuilt => "IndexRebuilt",
        AuditEvent::ReadOnlyModeEntered => "ReadOnlyModeEntered",
        AuditEvent::ReadOnlyModeLeft => "ReadOnlyModeLeft",
        AuditEvent::AuditWriteFailed => "AuditWriteFailed",
        AuditEvent::TapeRetired => "TapeRetired",
        AuditEvent::TapeProvisioned => "TapeProvisioned",
        AuditEvent::TapeIdentityAdopted => "TapeIdentityAdopted",
        AuditEvent::TapePoolAssigned => "TapePoolAssigned",
        AuditEvent::TapeSealed => "TapeSealed",
        AuditEvent::DriveRetired => "DriveRetired",
        AuditEvent::DriveReinstated => "DriveReinstated",
        AuditEvent::DriveAnnotated => "DriveAnnotated",
        AuditEvent::DriveCleaned => "DriveCleaned",
        AuditEvent::CleaningCartridgeExpired => "CleaningCartridgeExpired",
        AuditEvent::CleaningCartridgeRegistered => "CleaningCartridgeRegistered",
        AuditEvent::DriveFenced => "DriveFenced",
        AuditEvent::DriveUnfenced => "DriveUnfenced",
        AuditEvent::AlarmAcked => "AlarmAcked",
        AuditEvent::AlarmRaised => "AlarmRaised",
        AuditEvent::AlarmCleared => "AlarmCleared",
        AuditEvent::TapeIoFenceRaised => "TapeIoFenceRaised",
        AuditEvent::TapeIoFenceReleased => "TapeIoFenceReleased",
        AuditEvent::DriveHealthObserved => "DriveHealthObserved",
    }
}
