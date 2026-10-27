//! Shared daemon readiness waits for session opens and tape verification.

use crate::{status_error, DaemonClientError};
use remanence_api::pb;
use std::io::Write;
use std::time::Duration;
use tonic::transport::Channel;
use uuid::Uuid;

// Media-readiness waits mirror remfield-io: a cold library may need minutes
// of load/position work before the first open succeeds.
pub(crate) const DEFAULT_READY_TIMEOUT_SECONDS: u32 = 9_000;
const DEFAULT_READY_POLL_SECONDS: u32 = 30;
const DRIVE_BUSY_RETRY_DELAY: Duration = Duration::from_secs(1);

/// The daemon reports readiness fences as FailedPrecondition message tokens.
/// It supplies no structured status detail for these refusals.
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
    client
        .resume(pb::ResumeMediaReadinessRequest {
            operation_id: operation_id.as_bytes().to_vec(),
            timeout_seconds,
            poll_interval_seconds: DEFAULT_READY_POLL_SECONDS.min(timeout_seconds).max(1),
        })
        .await
        .map_err(status_error)?;
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
}
