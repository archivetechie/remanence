//! Shared daemon readiness waits for session opens and tape verification.

use crate::{status_error, DaemonClientError};
use remanence_api::pb;
use std::io::Write;
use std::time::Duration;
use tonic::transport::Channel;
use uuid::Uuid;

use remanence_api::readiness_wait::DEFAULT_READY_POLL_SECONDS;
pub(crate) use remanence_api::readiness_wait::DEFAULT_READY_TIMEOUT_SECONDS;
const DRIVE_BUSY_RETRY_DELAY: Duration = Duration::from_secs(1);

/// The daemon reports readiness fences as FailedPrecondition message tokens.
/// Structured metadata also carries the durable state when resume is refused.
pub(crate) fn readiness_operation(status: &tonic::Status) -> Option<Uuid> {
    retryable_precondition(status)
        .then(|| {
            media_readiness_operation_id(status.message())
                .or_else(|| held_readiness_operation(status))
        })
        .flatten()
}

/// Identify the readiness operation whose drive reservation another caller holds.
fn held_readiness_operation(status: &tonic::Status) -> Option<Uuid> {
    if !retryable_precondition(status) {
        return None;
    }
    let tail = status
        .message()
        .split_once("another caller holds this drive while media-readiness operation ")?
        .1;
    let (operation, _) = tail.split_once(" is unresolved")?;
    Uuid::parse_str(operation).ok()
}

fn retryable_precondition(status: &tonic::Status) -> bool {
    status.code() == tonic::Code::FailedPrecondition
        && !status.message().contains("resume append error:")
}

/// Readiness RPC boundary; fake clients exercise waits without drive access.
pub(crate) trait ReadinessClient {
    async fn resume(
        &mut self,
        request: pb::ResumeMediaReadinessRequest,
    ) -> Result<(), tonic::Status>;
    async fn watch(&mut self, operation_id: Uuid) -> Result<(), tonic::Status>;
    async fn next(&mut self) -> Result<Option<pb::OperationStatus>, tonic::Status>;
}

pub(crate) struct DaemonReadinessClient {
    channel: Channel,
    stream: Option<tonic::Streaming<pb::OperationStatus>>,
}

impl DaemonReadinessClient {
    pub(crate) fn new(channel: Channel) -> Self {
        Self {
            channel,
            stream: None,
        }
    }
}

impl ReadinessClient for DaemonReadinessClient {
    async fn resume(
        &mut self,
        request: pb::ResumeMediaReadinessRequest,
    ) -> Result<(), tonic::Status> {
        let operation = pb::library_service_client::LibraryServiceClient::new(self.channel.clone())
            .resume_media_readiness(request)
            .await?
            .into_inner();
        self.start_watch(operation.operation_id).await
    }

    async fn watch(&mut self, operation_id: Uuid) -> Result<(), tonic::Status> {
        self.start_watch(operation_id.as_bytes().to_vec()).await
    }

    async fn next(&mut self) -> Result<Option<pb::OperationStatus>, tonic::Status> {
        self.stream
            .as_mut()
            .expect("resume or watch starts the operation stream")
            .message()
            .await
    }
}

impl DaemonReadinessClient {
    /// Open the operation stream without acquiring the drive reservation.
    async fn start_watch(&mut self, operation_id: Vec<u8>) -> Result<(), tonic::Status> {
        self.stream = Some(
            pb::daemon_client::DaemonClient::new(self.channel.clone())
                .watch_operation(pb::GetOperationRequest { operation_id })
                .await?
                .into_inner(),
        );
        Ok(())
    }
}

/// Mirror of the remfield-io open-retry contract: a FailedPrecondition that
/// names a media-readiness operation means a tape load is in flight — watch it
/// to completion, then retry once. A busy drive bay gets one short retry.
/// Anything else is a real error.
pub(crate) async fn wait_before_open_retry(
    channel: Channel,
    status: &tonic::Status,
    err: &mut dyn Write,
) -> Result<(), DaemonClientError> {
    if !retryable_precondition(status) {
        return Err(status_error(status.clone()));
    }
    if let Some(operation_id) = media_readiness_operation_id(status.message()) {
        let _ = writeln!(
            err,
            "open fenced by media readiness operation {operation_id}; waiting \
             up to {DEFAULT_READY_TIMEOUT_SECONDS}s (pass --no-wait to fail fast)",
        );
        return wait_for_media_readiness_operation(
            &mut DaemonReadinessClient::new(channel),
            operation_id,
            DEFAULT_READY_TIMEOUT_SECONDS,
            false,
            err,
        )
        .await;
    }
    if wait_for_busy_drive_retry(status, "open", err).await {
        return Ok(());
    }
    Err(status_error(status.clone()))
}

/// Give a busy drive reservation one second to clear before a caller's single retry.
pub(crate) async fn wait_for_busy_drive_retry(
    status: &tonic::Status,
    action: &str,
    err: &mut dyn Write,
) -> bool {
    if !retryable_precondition(status) || !drive_bay_busy(status.message()) {
        return false;
    }
    let _ = writeln!(
        err,
        "{action} found a busy drive bay; retrying once after {:.0}s",
        DRIVE_BUSY_RETRY_DELAY.as_secs_f64()
    );
    tokio::time::sleep(DRIVE_BUSY_RETRY_DELAY).await;
    true
}

fn media_readiness_operation_id(message: &str) -> Option<Uuid> {
    if !message.contains("media-readiness") {
        return None;
    }
    message.split_ascii_whitespace().find_map(|token| {
        let value = token.strip_prefix("operation=")?;
        Uuid::parse_str(value.trim_matches(|ch: char| !ch.is_ascii_hexdigit() && ch != '-')).ok()
    })
}

fn drive_bay_busy(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    (lower.contains("drive bay") && lower.contains("is busy"))
        || lower.contains("drive-session owner is busy")
}

/// Resume and watch a readiness operation through the daemon's recorded states.
pub(crate) async fn wait_for_media_readiness_operation(
    client: &mut impl ReadinessClient,
    operation_id: Uuid,
    timeout_seconds: u32,
    state_changes_only: bool,
    err: &mut dyn Write,
) -> Result<(), DaemonClientError> {
    if let Err(error) = client
        .resume(pb::ResumeMediaReadinessRequest {
            operation_id: operation_id.as_bytes().to_vec(),
            timeout_seconds,
            poll_interval_seconds: DEFAULT_READY_POLL_SECONDS.min(timeout_seconds).max(1),
        })
        .await
    {
        if readiness_state(&error) == Some("ready") {
            return Ok(());
        }
        return Err(status_error(error));
    }
    watch_media_readiness_operation(client, operation_id, state_changes_only, err).await
}

/// Resume with the daemon's long timeout, or watch the caller that holds the drive.
pub(crate) async fn wait_for_verification_readiness(
    client: &mut impl ReadinessClient,
    operation_id: Uuid,
    refusal: &tonic::Status,
    err: &mut dyn Write,
) -> Result<(), DaemonClientError> {
    if held_readiness_operation(refusal) == Some(operation_id) {
        client.watch(operation_id).await.map_err(status_error)?;
    } else if let Err(error) = client
        .resume(pb::ResumeMediaReadinessRequest {
            operation_id: operation_id.as_bytes().to_vec(),
            timeout_seconds: DEFAULT_READY_TIMEOUT_SECONDS,
            poll_interval_seconds: DEFAULT_READY_POLL_SECONDS,
        })
        .await
    {
        if readiness_state(&error) == Some("ready") {
            return Ok(());
        }
        if held_readiness_operation(&error) != Some(operation_id) {
            return Err(status_error(error));
        }
        client.watch(operation_id).await.map_err(status_error)?;
    }
    watch_media_readiness_operation(client, operation_id, true, err).await
}

/// Consume readiness states until success, failure or the end of the stream.
async fn watch_media_readiness_operation(
    client: &mut impl ReadinessClient,
    operation_id: Uuid,
    state_changes_only: bool,
    err: &mut dyn Write,
) -> Result<(), DaemonClientError> {
    let mut last_readiness = None;
    while let Some(status) = client.next().await.map_err(status_error)? {
        let state =
            pb::OperationState::try_from(status.state).unwrap_or(pb::OperationState::Unspecified);
        let readiness = status
            .progress
            .get("state")
            .map(String::as_str)
            .unwrap_or("unknown");
        let reason = match (
            status
                .progress
                .get(remanence_library::MEDIA_READINESS_SENSE_PROGRESS_KEYS.0),
            status
                .progress
                .get(remanence_library::MEDIA_READINESS_SENSE_PROGRESS_KEYS.1),
        ) {
            (Some(sense), Some(name)) => format!(" sense={sense} ({name})"),
            (Some(sense), None) => format!(" sense={sense}"),
            _ => String::new(),
        };
        if !state_changes_only || last_readiness.as_deref() != Some(readiness) {
            let _ = writeln!(
                err,
                "media readiness operation {operation_id}: {readiness}{reason}"
            );
            last_readiness = Some(readiness.to_owned());
        }
        match state {
            pb::OperationState::Succeeded => return Ok(()),
            pb::OperationState::Failed
            | pb::OperationState::Cancelled
            | pb::OperationState::CompletionUnknown => {
                let summary = if status.error_summary.is_empty() {
                    format!("finished {state:?}")
                } else {
                    status.error_summary
                };
                return Err(DaemonClientError::client(format!(
                    "media readiness operation {operation_id} did not reach READY: {summary}"
                )));
            }
            _ => {}
        }
    }
    Err(DaemonClientError::client(format!(
        "media readiness operation {operation_id} watch ended before READY"
    )))
}

/// Attach to a daemon wait; the command deadline never cancels the drive wait.
pub(crate) fn run_daemon_wait_ready(
    endpoint: &str,
    operation: Uuid,
    timeout: Duration,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::process::ExitCode {
    let result = (|| {
        let runtime = crate::daemon_runtime().map_err(|e| e.message)?;
        runtime.block_on(async {
            let channel = crate::connect_daemon(endpoint).await?;
            let mut client = DaemonReadinessClient::new(channel);
            Ok::<_, String>(watch_daemon_wait_ready(&mut client, operation, timeout, err).await)
        })
    })();
    let code = match result {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            40
        }
    };
    if json {
        let _ = writeln!(
            out,
            "{}",
            serde_json::json!({"schema": "rem.tape.wait_ready_resume.v1", "operation_id": operation.to_string(), "exit_code": code})
        );
    }
    std::process::ExitCode::from(code)
}

/// Read the durable classification when a fence completes before a resume arrives.
fn readiness_state(status: &tonic::Status) -> Option<&str> {
    status
        .metadata()
        .get("remanence-readiness-state")
        .and_then(|value| value.to_str().ok())
}

/// Map daemon readiness state and operation outcomes to the wait-ready exit table.
fn daemon_wait_ready_exit(status: &pb::OperationStatus) -> Option<u8> {
    let state = status
        .progress
        .get("state")
        .map(String::as_str)
        .unwrap_or("");
    if status.state == pb::OperationState::Cancelled as i32
        && status
            .progress
            .get("phase")
            .is_some_and(|phase| phase == "shutdown")
        && remanence_api::readiness_wait::is_resumable(state)
    {
        return Some(10);
    }
    match pb::OperationState::try_from(status.state) {
        Ok(pb::OperationState::Succeeded) => Some(0),
        Ok(
            pb::OperationState::Failed
            | pb::OperationState::Cancelled
            | pb::OperationState::CompletionUnknown,
        ) => Some(match state {
            "timeout_unknown" => 20,
            "transport_unknown" | "aborted_unknown" => 40,
            "reservation_conflict" => 50,
            _ => 30,
        }),
        _ => None,
    }
}

/// Print each readiness state once per change while watching the attached operation.
async fn watch_daemon_wait_ready(
    client: &mut impl ReadinessClient,
    operation: Uuid,
    timeout: Duration,
    err: &mut dyn Write,
) -> u8 {
    let watch = async {
        if let Err(error) = client
            .resume(pb::ResumeMediaReadinessRequest {
                operation_id: operation.as_bytes().to_vec(),
                timeout_seconds: DEFAULT_READY_TIMEOUT_SECONDS,
                poll_interval_seconds: DEFAULT_READY_POLL_SECONDS,
            })
            .await
        {
            if readiness_state(&error) == Some("ready") {
                let _ = writeln!(err, "media readiness operation {operation}: ready");
                return 0;
            }
            let _ = writeln!(err, "{error}");
            if let Some(state) = readiness_state(&error) {
                return match state {
                    "timeout_unknown" => 20,
                    "transport_unknown" | "aborted_unknown" => 40,
                    "reservation_conflict" => 50,
                    _ => 30,
                };
            }
            return match error.code() {
                tonic::Code::PermissionDenied
                | tonic::Code::Unauthenticated
                | tonic::Code::NotFound
                | tonic::Code::InvalidArgument
                | tonic::Code::FailedPrecondition => 50,
                tonic::Code::DeadlineExceeded => 20,
                tonic::Code::Unavailable | tonic::Code::Unknown => 40,
                _ => 30,
            };
        }
        let mut previous = None;
        loop {
            let status = match client.next().await {
                Ok(Some(status)) => status,
                Ok(None) => {
                    let _ = writeln!(err, "readiness watch ends before a terminal state");
                    return 40;
                }
                Err(error) => {
                    let _ = writeln!(err, "{error}");
                    return if error.code() == tonic::Code::DeadlineExceeded {
                        20
                    } else {
                        40
                    };
                }
            };
            let state = status
                .progress
                .get("state")
                .map(String::as_str)
                .unwrap_or("unknown");
            if previous.as_deref() != Some(state) {
                let (sense_key, name_key) = remanence_library::MEDIA_READINESS_SENSE_PROGRESS_KEYS;
                let sense = status
                    .progress
                    .get(sense_key)
                    .map(String::as_str)
                    .unwrap_or("");
                let name = status
                    .progress
                    .get(name_key)
                    .map(String::as_str)
                    .unwrap_or("");
                let _ = writeln!(
                    err,
                    "media readiness operation {operation}: {state} sense={sense} {name}"
                );
                previous = Some(state.to_owned());
            }
            if let Some(code) = daemon_wait_ready_exit(&status) {
                if !status.error_summary.is_empty() {
                    let _ = writeln!(err, "{}", status.error_summary);
                }
                return code;
            }
        }
    };
    tokio::time::timeout(timeout, watch).await.unwrap_or(20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_suffix_does_not_expand_busy_retry_policy() {
        assert!(drive_bay_busy(
            "drive bay 0x0100 is busy; held by: drive A held by tape_verification 1"
        ));
        for message in [
            "no free drive bay available to load cartridge X",
            "all drives are busy",
            "drives are busy",
        ] {
            assert!(!drive_bay_busy(message));
            assert!(!drive_bay_busy(&format!(
                "{message}; held by: drive A held by tape_verification 1"
            )));
        }
    }

    /// Admission, robotics and verification refusals identify the same operation.
    #[test]
    fn parses_daemon_readiness_refusals() {
        let operation = Uuid::from_u128(1);
        let admission = format!("open is blocked by active media-readiness fence quarantine operation={operation} library=LIB drive=0x0100 barcode=TAPE state=media_initializing");
        let robotics = format!("{admission}; robot moves in library LIB are refused while that fence is active, so this request needs operation {operation} to finish first, even when other drive bays are idle");
        let verification = format!("fully verify terminal tape index blocked by media-readiness fence operation={operation} library=LIB drive=0x0100 media_readiness_state=media_initializing");
        for message in [admission, robotics, verification] {
            assert_eq!(
                readiness_operation(&tonic::Status::failed_precondition(&message)),
                Some(operation)
            );
            assert_eq!(
                readiness_operation(&tonic::Status::unavailable(&message)),
                None
            );
        }
        for message in [
            "blocked by media-readiness fence operation=invalid",
            "drive bay 0x0100 is busy",
            "another caller holds this drive while media-readiness operation invalid is unresolved",
        ] {
            assert_eq!(
                readiness_operation(&tonic::Status::failed_precondition(message)),
                None
            );
        }
    }
    /// A fake stream keeps the daemon wait independent of the CLI deadline.
    struct FakeReadiness {
        events: std::collections::VecDeque<pb::OperationStatus>,
        resumes: Vec<pb::ResumeMediaReadinessRequest>,
        resume_error: Option<tonic::Status>,
    }
    impl ReadinessClient for FakeReadiness {
        async fn resume(
            &mut self,
            request: pb::ResumeMediaReadinessRequest,
        ) -> Result<(), tonic::Status> {
            self.resumes.push(request);
            match &self.resume_error {
                Some(error) => Err(error.clone()),
                None => Ok(()),
            }
        }
        async fn watch(&mut self, _id: Uuid) -> Result<(), tonic::Status> {
            Ok(())
        }
        async fn next(&mut self) -> Result<Option<pb::OperationStatus>, tonic::Status> {
            if let Some(status) = self.events.pop_front() {
                return Ok(Some(status));
            }
            std::future::pending().await
        }
    }

    /// Daemon attachment uses the long wait and maps every command outcome.
    #[tokio::test]
    async fn daemon_wait_ready_attach_exit_codes_and_state_changes() {
        for (state, operation_state, expected) in [
            ("ready", pb::OperationState::Succeeded, 0),
            ("becoming_ready", pb::OperationState::Running, 20),
            ("timeout_unknown", pb::OperationState::Failed, 20),
            ("terminal_error", pb::OperationState::Failed, 30),
            ("transport_unknown", pb::OperationState::Failed, 40),
            ("reservation_conflict", pb::OperationState::Failed, 50),
        ] {
            let event = |state: &str, op| pb::OperationStatus {
                state: op as i32,
                progress: std::collections::HashMap::from([("state".into(), state.into())]),
                ..Default::default()
            };
            let mut client = FakeReadiness {
                events: [
                    event("becoming_ready", pb::OperationState::Running),
                    event("becoming_ready", pb::OperationState::Running),
                    event(state, operation_state),
                ]
                .into(),
                resumes: vec![],
                resume_error: None,
            };
            let mut err = vec![];
            assert_eq!(
                watch_daemon_wait_ready(
                    &mut client,
                    Uuid::new_v4(),
                    Duration::from_millis(20),
                    &mut err
                )
                .await,
                expected
            );
            assert_eq!(client.resumes.len(), 1);
            assert_eq!(client.resumes[0].timeout_seconds, 9000);
            assert_eq!(client.resumes[0].poll_interval_seconds, 30);
            let lines = String::from_utf8(err).unwrap();
            assert_eq!(lines.matches(": becoming_ready sense=").count(), 1);
        }
    }

    /// Shutdown of a resumable wait uses the still-not-ready exit code.
    #[tokio::test]
    async fn daemon_wait_ready_shutdown_preserves_resumable_exit() {
        for readiness in [
            "planned",
            "pre_ready_loading",
            "media_initializing",
            "becoming_ready",
            "target_busy",
            "unit_attention",
        ] {
            let mut client = FakeReadiness {
                events: [pb::OperationStatus {
                    state: pb::OperationState::Cancelled as i32,
                    progress: std::collections::HashMap::from([
                        ("state".into(), readiness.into()),
                        ("phase".into(), "shutdown".into()),
                    ]),
                    ..Default::default()
                }]
                .into(),
                resumes: vec![],
                resume_error: None,
            };
            assert_eq!(
                watch_daemon_wait_ready(
                    &mut client,
                    Uuid::new_v4(),
                    Duration::from_secs(1),
                    &mut vec![]
                )
                .await,
                10
            );
        }
        let status = pb::OperationStatus {
            state: pb::OperationState::Cancelled as i32,
            progress: std::collections::HashMap::from([
                ("state".into(), "becoming_ready".into()),
                ("phase".into(), "cancelled".into()),
            ]),
            ..Default::default()
        };
        assert_eq!(daemon_wait_ready_exit(&status), Some(30));
    }

    /// Open retry consumes the same attached wait for either initiating caller.
    #[tokio::test]
    async fn open_retry_watches_attached_wait_from_either_initiator() {
        for started_by in ["daemon", "client"] {
            let mut client = FakeReadiness {
                events: [pb::OperationStatus {
                    state: pb::OperationState::Succeeded as i32,
                    progress: std::collections::HashMap::from([
                        ("state".into(), "ready".into()),
                        ("started_by".into(), started_by.into()),
                    ]),
                    ..Default::default()
                }]
                .into(),
                resumes: vec![],
                resume_error: None,
            };
            wait_for_media_readiness_operation(
                &mut client,
                Uuid::new_v4(),
                DEFAULT_READY_TIMEOUT_SECONDS,
                false,
                &mut vec![],
            )
            .await
            .unwrap();
            assert_eq!(client.resumes.len(), 1);
        }
    }
    /// Completion between the refused open and resume still permits the retry.
    #[tokio::test]
    async fn readiness_completes_before_resume_retries_open() {
        let mut error =
            tonic::Status::failed_precondition("operation is ready and cannot be resumed");
        error
            .metadata_mut()
            .insert("remanence-readiness-state", "ready".parse().unwrap());
        let mut client = FakeReadiness {
            events: Default::default(),
            resumes: vec![],
            resume_error: Some(error),
        };
        let id = Uuid::new_v4();
        wait_for_media_readiness_operation(&mut client, id, 9000, false, &mut vec![])
            .await
            .unwrap();
        wait_for_verification_readiness(
            &mut client,
            id,
            &tonic::Status::failed_precondition("fence"),
            &mut vec![],
        )
        .await
        .unwrap();
        assert_eq!(
            watch_daemon_wait_ready(&mut client, id, Duration::from_secs(1), &mut vec![]).await,
            0
        );
    }
    /// Resume has its own JSON shape and never advertises the direct-device schema.
    #[test]
    fn daemon_wait_ready_json_uses_resume_schema() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let id = Uuid::new_v4();
        assert_eq!(
            run_daemon_wait_ready(
                "http://[",
                id,
                Duration::from_secs(1),
                true,
                &mut out,
                &mut err
            ),
            std::process::ExitCode::from(40)
        );
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["schema"], "rem.tape.wait_ready_resume.v1");
        assert_eq!(value["operation_id"], id.to_string());
        assert_eq!(value["exit_code"], 40);
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("invalid daemon endpoint"));
    }

    /// Timeouts before a watch starts use the same exit table as observed timeouts.
    #[tokio::test]
    async fn daemon_wait_ready_maps_resume_timeouts() {
        let mut stored = tonic::Status::failed_precondition("wait timed out");
        stored.metadata_mut().insert(
            "remanence-readiness-state",
            "timeout_unknown".parse().unwrap(),
        );
        for error in [stored, tonic::Status::deadline_exceeded("resume timed out")] {
            let mut client = FakeReadiness {
                events: Default::default(),
                resumes: vec![],
                resume_error: Some(error),
            };
            assert_eq!(
                watch_daemon_wait_ready(
                    &mut client,
                    Uuid::new_v4(),
                    Duration::from_secs(1),
                    &mut vec![]
                )
                .await,
                20
            );
        }
    }
}
