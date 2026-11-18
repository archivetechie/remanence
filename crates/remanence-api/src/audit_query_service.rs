//! Read-only operation history over the authoritative library log.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::pin::Pin;

use remanence_state::library_runtime::{
    HistoryEnd, HistoryEntry, HistoryHead, LibraryRuntime, RuntimeError,
};
use remanence_state::{AuditActor, SourceLayer};
use time::OffsetDateTime;
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::auth::{authorize_request, AuthPermission};
use crate::catalog_conversion::send_stream_item;
use crate::pb;

const AUDIT_STREAM_BUFFER: usize = 32;
type AuditEntryStream =
    Pin<Box<dyn Stream<Item = Result<pb::AuditEntry, Status>> + Send + 'static>>;

/// Read-only Layer 5 history service over the authoritative library log.
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
        let runtime = self.state.catalog.library_runtime().ok_or_else(|| {
            Status::failed_precondition(
                "audit history reads the library log, and this service has no library authority",
            )
        })?;
        Ok(Response::new(audit_entry_stream(runtime, query)))
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

/// Walk a captured library prefix on a blocking worker; filter before rendering detail.
pub(crate) fn audit_entry_stream(runtime: LibraryRuntime, query: AuditQuery) -> AuditEntryStream {
    let (tx, rx) = tokio::sync::mpsc::channel(AUDIT_STREAM_BUFFER);
    tokio::task::spawn_blocking(move || {
        let mut last = 0;
        let result = runtime.history(&mut |entry| {
            last = entry.head().sequence;
            if tx.is_closed() {
                return ControlFlow::Break(());
            }
            if !audit_head_matches(entry.head(), &query) {
                return ControlFlow::Continue(());
            }
            send_stream_item(&tx, Ok(entry_to_proto(entry)))
        });
        if let Some(status) = history_result_status(result, last) {
            let _ = tx.blocking_send(Err(status));
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

/// Preserve the walk's readable prefix, then report its exact terminal standing.
fn history_result_status(result: Result<HistoryEnd, RuntimeError>, last: u64) -> Option<Status> {
    match result {
        Ok(HistoryEnd::Clean | HistoryEnd::Stopped) => None,
        Ok(HistoryEnd::Fenced) => Some(Status::failed_precondition(format!(
            "audit history ends at library record {last}: the library log is fenced after a write whose outcome is unknown, and later records are not shown until it is recovered (rem-debug library-log recover)"
        ))),
        Ok(HistoryEnd::Refused { refusal }) => Some(Status::failed_precondition(format!(
            "audit history ends at library record {last}: the library log refused a record and needs recovery: {refusal}"
        ))),
        Ok(HistoryEnd::Damaged { segment, offset, fault }) => Some(Status::data_loss(format!(
            "audit history stopped after library record {last}: library log segment {segment} at offset {offset}: {fault}"
        ))),
        Err(error) => Some(Status::internal(format!(
            "audit history could not read the library log's standing: {error}"
        ))),
    }
}

/// Compare structured history fields without rendering the record body.
pub(crate) fn audit_head_matches(head: &HistoryHead, query: &AuditQuery) -> bool {
    let timestamp = i128::from(head.timestamp_unix_ns);
    if query
        .since
        .is_some_and(|since| timestamp < since.unix_timestamp_nanos())
        || query
            .until
            .is_some_and(|until| timestamp >= until.unix_timestamp_nanos())
    {
        return false;
    }
    for (key, expected) in &query.filter {
        let matched = match key.as_str() {
            "session_id" => head
                .session_id
                .is_some_and(|value| Uuid::from_bytes(value).to_string() == *expected),
            "operation_id" => head
                .operation_id
                .is_some_and(|value| Uuid::from_bytes(value).to_string() == *expected),
            "event_kind" | "event" | "kind" => {
                head.event_kind == *expected
                    || (!expected.contains('.')
                        && head.event_kind.starts_with(&format!("{expected}.")))
            }
            "actor" => audit_actor_name(&head.actor) == *expected,
            "source_layer" => audit_source_layer_name(&head.source_layer) == expected,
            "subject_kind" => head.subject_kind == *expected,
            "subject_id" => audit_subject_id_matches(head.subject_id.as_deref(), expected),
            _ => unreachable!("audit filter keys are validated before streaming"),
        };
        if !matched {
            return false;
        }
    }
    true
}

pub(crate) fn audit_subject_id_matches(actual: Option<&str>, expected: &str) -> bool {
    actual.is_some_and(|actual| {
        actual == expected
            || Uuid::parse_str(actual).is_ok_and(|actual_uuid| {
                Uuid::parse_str(expected).is_ok_and(|expected_uuid| actual_uuid == expected_uuid)
            })
    })
}

/// Convert one matched record, preserving the envelope's time, UUIDs and writer build.
fn entry_to_proto(entry: &HistoryEntry<'_>) -> pb::AuditEntry {
    let head = entry.head();
    pb::AuditEntry {
        sequence: head.sequence,
        timestamp: Some(prost_types::Timestamp {
            seconds: (head.timestamp_unix_ns / 1_000_000_000) as i64,
            nanos: (head.timestamp_unix_ns % 1_000_000_000) as i32,
        }),
        actor: audit_actor_name(&head.actor),
        source_layer: audit_source_layer_name(&head.source_layer).to_string(),
        operation_id: head
            .operation_id
            .map(|value| value.to_vec())
            .unwrap_or_default(),
        session_id: head
            .session_id
            .map(|value| value.to_vec())
            .unwrap_or_default(),
        event_kind: head.event_kind.clone(),
        detail_json: entry.detail_json(),
        software_build: Some(head.software_build.clone()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::TypedRoot;
    use remanence_state::library_log as lib;
    use tokio_stream::StreamExt;
    use tonic::Code;

    /// Exercise the same terminal mapping as the blocking stream worker, with exact wire text.
    #[test]
    fn audit_history_result_statuses_are_exact() {
        assert!(history_result_status(Ok(HistoryEnd::Clean), 7).is_none());
        assert!(history_result_status(Ok(HistoryEnd::Stopped), 7).is_none());
        for (result, code, message) in [
            (Ok(HistoryEnd::Fenced), Code::FailedPrecondition,
             "audit history ends at library record 7: the library log is fenced after a write whose outcome is unknown, and later records are not shown until it is recovered (rem-debug library-log recover)"),
            (Ok(HistoryEnd::Refused { refusal: "fixture refusal".into() }), Code::FailedPrecondition,
             "audit history ends at library record 7: the library log refused a record and needs recovery: fixture refusal"),
            (Ok(HistoryEnd::Damaged { segment: "fixture.remlib".into(), offset: 84, fault: "fixture fault".into() }), Code::DataLoss,
             "audit history stopped after library record 7: library log segment fixture.remlib at offset 84: fixture fault"),
            (Err(RuntimeError::MotionFenced), Code::Internal,
             "audit history could not read the library log's standing: library authority failed; motion is fenced until startup reconciliation"),
        ] {
            let status = history_result_status(result, 7);
            assert!(status.is_some(), "expected terminal status for {code:?}");
            let status = status.unwrap();
            assert_eq!(status.code(), code);
            assert_eq!(status.message(), message);
        }
    }

    /// Seed only through a real typed writer; history remains the sole read path.
    fn seed(runtime: &LibraryRuntime, subject_kind: &str) {
        runtime
            .append(|authority, view| {
                authority.diagnostic(
                    lib::Diagnostic {
                        event: lib::DiagnosticEvent::RequestReceived,
                        subject_kind: subject_kind.into(),
                        subject_id: None,
                        detail: lib::Bounded(vec![]),
                    },
                    AuditActor::System,
                    SourceLayer::Layer5,
                    view,
                )
            })
            .unwrap();
    }

    fn query() -> AuditQuery {
        AuditQuery::try_from(pb::QueryAuditRequest {
            filter: [("subject_kind".into(), "visible".into())].into(),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn audit_stream_fenced_delivers_prefix_then_last_visited_record() {
        let root = TypedRoot::open();
        let runtime = root.catalog().library_runtime().unwrap();
        seed(&runtime, "visible");
        seed(&runtime, "hidden");
        std::fs::write(root.paths().audit_dir.join("library-log.ambiguous"), []).unwrap();
        let mut stream = audit_entry_stream(runtime, query());
        let entry = stream.next().await.unwrap().unwrap();
        assert_eq!(entry.sequence, 1);
        assert_eq!(entry.event_kind, "Diagnostic.RequestReceived");
        let status = stream.next().await.unwrap().unwrap_err();
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(status.message(), "audit history ends at library record 2: the library log is fenced after a write whose outcome is unknown, and later records are not shown until it is recovered (rem-debug library-log recover)");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn audit_stream_damage_delivers_prefix_then_last_visited_record() {
        let root = TypedRoot::open();
        let runtime = root.catalog().library_runtime().unwrap();
        seed(&runtime, "visible");
        seed(&runtime, "hidden");
        let segments: Vec<_> = std::fs::read_dir(&root.paths().audit_dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "remlib"))
            .collect();
        assert_eq!(segments.len(), 1);
        let path = &segments[0];
        let offset = std::fs::metadata(path).unwrap().len();
        seed(&runtime, "hidden");
        assert!(std::fs::metadata(path).unwrap().len() > offset);
        // Cut at a frame boundary after the runtime retained the third frame's full limit.
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_len(offset)
            .unwrap();
        let mut stream = audit_entry_stream(runtime, query());
        assert_eq!(stream.next().await.unwrap().unwrap().sequence, 1);
        let status = stream.next().await.unwrap().unwrap_err();
        assert_eq!(status.code(), Code::DataLoss);
        let segment = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(status.message(), format!("audit history stopped after library record 2: library log segment {segment} at offset {offset}: segment ends before its recorded limit"));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn audit_stream_fenced_with_no_records_reports_zero() {
        let root = TypedRoot::open();
        let runtime = root.catalog().library_runtime().unwrap();
        std::fs::write(root.paths().audit_dir.join("library-log.ambiguous"), []).unwrap();
        let mut stream = audit_entry_stream(runtime, query());
        let status = stream.next().await.unwrap().unwrap_err();
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(status.message(), "audit history ends at library record 0: the library log is fenced after a write whose outcome is unknown, and later records are not shown until it is recovered (rem-debug library-log recover)");
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn audit_stream_standing_read_error_is_internal_before_entries() {
        let root = TypedRoot::open();
        let runtime = root.catalog().library_runtime().unwrap();
        std::os::unix::fs::symlink(
            "library-log.ambiguous",
            root.paths().audit_dir.join("library-log.ambiguous"),
        )
        .unwrap();
        let error = runtime
            .history(&mut |_| panic!("standing error must precede visits"))
            .unwrap_err();
        let mut stream = audit_entry_stream(runtime, query());
        let status = stream.next().await.unwrap().unwrap_err();
        assert_eq!(status.code(), Code::Internal);
        assert_eq!(
            status.message(),
            format!("audit history could not read the library log's standing: {error}")
        );
        assert!(stream.next().await.is_none());
    }
}
