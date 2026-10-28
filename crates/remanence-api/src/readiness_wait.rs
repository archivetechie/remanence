//! Shared readiness admission, automatic resumption, and bounded operation waits.

use crate::{pb, ApiState};
use std::time::Duration;
use tonic::Status;
use uuid::Uuid;

/// Long readiness profile shared by daemon and CLI.
pub const DEFAULT_READY_TIMEOUT_SECONDS: u32 = 9_000;
/// Steady readiness poll interval after the fast ramp.
pub const DEFAULT_READY_POLL_SECONDS: u32 = 30;
/// Session-open wait budget for verification and manual finalization.
pub const DEFAULT_OPERATION_READY_WAIT_SECONDS: u64 = 900;
/// A shutdown interruption preserves the durable resumable state.
pub(crate) const SHUTDOWN_REASON: &str = "daemon shutdown stops readiness wait";

/// Active states whose readiness wait may resume without operator release.
pub fn is_resumable(state: &str) -> bool {
    matches!(
        state,
        "planned"
            | "pre_ready_loading"
            | "media_initializing"
            | "becoming_ready"
            | "target_busy"
            | "unit_attention"
    )
}

/// Typed fence identity carried through actor replies and RPC refusals.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReadinessFence(pub(crate) Uuid);
impl ReadinessFence {
    pub(crate) fn status(self, message: impl Into<String>) -> Status {
        let mut status = Status::failed_precondition(message);
        status.metadata_mut().insert_bin(
            "remanence-readiness-operation-bin",
            tonic::metadata::MetadataValue::from_bytes(self.0.as_bytes()),
        );
        status
    }
    pub(crate) fn from_status(status: &Status) -> Option<Self> {
        let bytes = status
            .metadata()
            .get_bin("remanence-readiness-operation-bin")?
            .to_bytes()
            .ok()?;
        Uuid::from_slice(&bytes).ok().map(Self)
    }
}

impl ApiState {
    /// Attach to the running wait or reserve and dispatch exactly one new wait.
    pub(crate) fn start_readiness_wait(
        &self,
        operation_id: Uuid,
        timeout_seconds: u32,
        poll_interval_seconds: u32,
        started_by: &str,
        origin: &str,
    ) -> Result<pb::OperationRef, Status> {
        self.start_readiness_wait_with_index(
            None,
            operation_id,
            timeout_seconds,
            poll_interval_seconds,
            started_by,
            origin,
        )
    }

    /// Reuse the discovery catalog while retaining serialized admission checks.
    fn start_readiness_wait_with_index(
        &self,
        index: Option<&remanence_state::CatalogIndex>,
        operation_id: Uuid,
        timeout_seconds: u32,
        poll_interval_seconds: u32,
        started_by: &str,
        origin: &str,
    ) -> Result<pb::OperationRef, Status> {
        #[cfg(test)]
        if let Some(pool) = &self.drive_pool {
            pool.readiness_wake
                .start_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let _start_guard = self
            .operations
            .readiness_start
            .lock()
            .expect("readiness start lock");
        if self
            .readiness_shutdown
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(Status::unavailable(
                "daemon shutdown stops readiness admission",
            ));
        }
        if let Some(status) = self.operations.latest(&operation_id) {
            if matches!(
                status.operation_kind.as_str(),
                "media_readiness_wait" | "load_drive"
            ) && !crate::operations::is_terminal(&status)
            {
                return Ok(pb::OperationRef {
                    operation_id: operation_id.as_bytes().to_vec(),
                });
            }
        }
        if self.operations.is_live(&operation_id) {
            return Err(Status::failed_precondition(
                "another operation owns this readiness id",
            ));
        }
        let opened;
        let index = match index {
            Some(index) => index,
            None => {
                opened = self.index()?;
                &opened
            }
        };
        let record = index
            .media_readiness_operation(operation_id)
            .map_err(crate::startup_media_readiness::status_from_state_error)?
            .ok_or_else(|| Status::not_found("media-readiness operation not found"))?;
        if started_by == "daemon" && record.cancel_source.as_deref() == Some("operator") {
            return Err(Status::failed_precondition(
                "readiness wait is cancelled by an operator",
            ));
        }
        if !crate::readiness_wait::is_resumable(&record.state) {
            let mut status = Status::failed_precondition(format!(
                "media-readiness operation {operation_id} is state {} and cannot be resumed",
                record.state
            ));
            if let Ok(value) = record.state.parse() {
                status
                    .metadata_mut()
                    .insert("remanence-readiness-state", value);
            }
            return Err(status);
        }
        let remanence_state::DriveKey::Bay {
            library_serial,
            bay,
        } = &record.endpoint
        else {
            return Err(Status::failed_precondition(
                "this daemon operates library endpoints only",
            ));
        };
        let bay = *bay;
        if !self.operates_library(library_serial) {
            return Err(Status::failed_precondition(format!("media-readiness operation {operation_id} belongs to library {library_serial}, which this daemon does not operate")));
        }
        let snapshot = self
            .current_library_snapshot()
            .ok_or_else(|| Status::not_found("library snapshot unavailable"))?;
        let library = snapshot
            .report
            .library(library_serial)
            .ok_or_else(|| Status::not_found("media-readiness library not found"))?;
        let drive_bay = library
            .drive_bays
            .iter()
            .find(|candidate| candidate.element_address == bay)
            .ok_or_else(|| Status::not_found("media-readiness drive bay not found"))?;
        if let Some(expected) = record.drive_serial.as_deref() {
            let actual = drive_bay
                .installed
                .as_ref()
                .map(|drive| drive.serial.as_str());
            if actual != Some(expected) {
                return Err(Status::failed_precondition(format!(
                    "media-readiness operation {operation_id} expected drive serial {expected}, snapshot has {}",
                    actual.unwrap_or("(none)")
                )));
            }
        }
        if let Some(expected) = record.barcode.as_deref() {
            if drive_bay.loaded_tape.as_deref() != Some(expected) {
                return Err(Status::failed_precondition(format!(
                    "media-readiness operation {operation_id} expected barcode {expected} in drive 0x{bay:04x}, snapshot has {}",
                    drive_bay.loaded_tape.as_deref().unwrap_or("(none)")
                )));
            }
        }

        let pool = self.drive_pool()?.clone();
        let drive_key = record.endpoint.clone();
        // One caller at a time holds a fenced drive: the wait below keeps the
        // reservation until it ends. A second caller is often a client whose
        // open the same fence refused because that open needs the library
        // robot, so the refusal names the serialization, not only the drive,
        // and the fence's last sense when it has one.
        let fence_sense =
            crate::startup_media_readiness::media_readiness_record_sense_label(&record)
                .map(|label| format!(", sense={label}"))
                .unwrap_or_default();
        let reservation = pool.reserve_drive(&drive_key).map_err(|error| {
            if error.code() == tonic::Code::FailedPrecondition
                && error.message().split("; held by: ").next() == Some(format!("{} is busy", drive_key.label()).as_str())
            {
                pool.append_holders(Status::failed_precondition(format!(
                    "{} is busy: another caller holds this drive while media-readiness operation {operation_id} is unresolved{fence_sense}. Robot moves in library {library_serial} are refused until that operation finishes, even when other drive bays are idle; retry when operation {operation_id} finishes",
                    drive_key.label()
                )), |key| key == &drive_key)
            } else {
                error
            }
        })?;
        pool.set_holder(
            &drive_key,
            drive_bay
                .installed
                .as_ref()
                .map(|drive| drive.serial.as_str()),
            "media_readiness_wait",
            operation_id,
        );
        let drive_tx = pool.drive_tx(&drive_key)?;
        let handle = self
            .operations
            .register(operation_id, "media_readiness_wait")
            .with_readiness_lifetime(self.readiness_shutdown.clone(), pool.readiness_wake.clone());
        handle.publish_state(
            pb::OperationState::Running,
            &[("started_by", started_by), ("state", &record.state)],
        );
        let family = if record
            .media_generation
            .is_some_and(|generation| generation >= 9)
        {
            remanence_library::MediaFamily::Lto9OrLater
        } else {
            remanence_library::MediaFamily::Unknown
        };
        let command = crate::write_owner::DriveCommand::WaitReady {
            operation_id,
            family,
            options: remanence_library::MediaReadinessWaitOptions {
                wait: true,
                timeout: Duration::from_secs(u64::from(timeout_seconds)),
                poll_interval: Duration::from_secs(u64::from(poll_interval_seconds)),
            },
            cartridge_in_drive: drive_bay.loaded,
            handle: handle.clone(),
            reservation,
        };
        match drive_tx.try_send(command) {
            Ok(()) => {
                if started_by == "daemon" {
                    tracing::info!(%operation_id, endpoint = %record.endpoint.label(), barcode = record.barcode.as_deref().unwrap_or("(unknown)"), state = %record.state, sense = %crate::startup_media_readiness::media_readiness_record_sense_label(&record).unwrap_or_default(), origin, "daemon starts media-readiness wait");
                }
                Ok(pb::OperationRef {
                    operation_id: operation_id.as_bytes().to_vec(),
                })
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                handle.publish_failed("drive-session owner is busy", &[("phase", "dispatch")]);
                Err(Status::failed_precondition("drive-session owner is busy"))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                handle.publish_failed(
                    "drive-session owner is stopped",
                    &[("phase", "dispatch"), ("state", "dispatch_failed")],
                );
                Err(Status::unavailable("drive-session owner is stopped"))
            }
        }
    }
}

/// Per-fence retry eligibility survives notifications and repeated observations.
struct DiscoveryRetry {
    state: String,
    live: bool,
    fast_until: Option<tokio::time::Instant>,
    next_attempt: tokio::time::Instant,
    refusals: std::collections::HashSet<String>,
}

/// Discover fences on changes and a fixed sweep. Only newly eligible or changed
/// records get a short busy retry window; failures wait for the sweep.
pub(crate) fn spawn_auto_resume(state: ApiState) {
    let Some(pool) = state.drive_pool.clone() else {
        return;
    };
    tokio::spawn(async move {
        let mut index = None;
        let mut startup = true;
        let mut retries = std::collections::HashMap::<Uuid, DiscoveryRetry>::new();
        let mut sweep_at = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if state
                .readiness_shutdown
                .load(std::sync::atomic::Ordering::Acquire)
            {
                break;
            }
            let now = tokio::time::Instant::now();
            let sweep = now >= sweep_at;
            if sweep {
                sweep_at = now + Duration::from_secs(30);
            }
            if index.is_none() {
                match state.index() {
                    Ok(opened) => index = Some(opened),
                    Err(error) => tracing::warn!(%error, "cannot open readiness catalog"),
                }
            }
            if let Some(index) = &index {
                #[cfg(test)]
                pool.readiness_wake
                    .scans
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                match index.list_active_media_readiness_operations(None) {
                    Ok(records) => {
                        let mut eligible = std::collections::HashSet::new();
                        for record in records {
                            if !is_resumable(&record.state)
                                || record.cancel_source.as_deref() == Some("operator")
                            {
                                continue;
                            }
                            let Ok(id) = Uuid::parse_str(&record.operation_id) else {
                                tracing::warn!(row = %record.operation_id, "invalid readiness operation id");
                                continue;
                            };
                            eligible.insert(id);
                            let live = state.operations.is_live(&id);
                            let retry = retries.entry(id).or_insert_with(|| DiscoveryRetry {
                                state: record.state.clone(),
                                live,
                                fast_until: Some(now + Duration::from_secs(5)),
                                next_attempt: now,
                                refusals: Default::default(),
                            });
                            if retry.state != record.state || (retry.live && !live) {
                                retry.state = record.state.clone();
                                retry.fast_until = Some(now + Duration::from_secs(5));
                                retry.next_attempt = now;
                            }
                            retry.live = live;
                            if live {
                                continue;
                            }
                            if state.operations.latest(&id).is_some_and(|status| {
                                status.operation_kind == "media_readiness_wait"
                                    && status.state == pb::OperationState::Failed as i32
                                    && (status
                                        .progress
                                        .get("phase")
                                        .is_none_or(|phase| phase != "dispatch")
                                        || status
                                            .progress
                                            .get("state")
                                            .is_some_and(|state| state == "dispatch_failed"))
                            }) {
                                retry.fast_until = None;
                            }
                            if retry.fast_until.is_some_and(|until| now >= until) {
                                retry.fast_until = None;
                            }
                            let fast = retry.fast_until.is_some();
                            if !(sweep || fast && now >= retry.next_attempt) {
                                continue;
                            }
                            match state.start_readiness_wait_with_index(
                                Some(index),
                                id,
                                DEFAULT_READY_TIMEOUT_SECONDS,
                                DEFAULT_READY_POLL_SECONDS,
                                "daemon",
                                if startup { "startup" } else { "session_open" },
                            ) {
                                Ok(_) => {
                                    retry.live = true;
                                    retry.fast_until = None;
                                }
                                Err(error) => {
                                    // Only reservation/dispatch congestion merits a fast retry.
                                    let busy = error.code() == tonic::Code::FailedPrecondition
                                        && (error.message().starts_with(&format!(
                                            "{} is busy",
                                            record.endpoint.label()
                                        )) || error.message() == "drive-session owner is busy");
                                    if !busy {
                                        retry.fast_until = None;
                                    }
                                    retry.next_attempt = now + Duration::from_millis(250);
                                    if retry.refusals.insert(format!(
                                        "{:?}:{}",
                                        error.code(),
                                        error.message()
                                    )) {
                                        tracing::warn!(operation_id = %id, %error, "automatic readiness start cannot dispatch");
                                    }
                                }
                            }
                        }
                        retries.retain(|id, _| eligible.contains(id));
                    }
                    Err(error) => {
                        // A failed scan cannot leave an overdue fast timer spinning.
                        for retry in retries.values_mut() {
                            retry.fast_until = None;
                        }
                        tracing::warn!(%error, "cannot read readiness fences");
                    }
                }
            }
            startup = false;
            let next = retries
                .values()
                .filter(|retry| !retry.live)
                .filter_map(|retry| {
                    retry
                        .fast_until
                        .filter(|until| retry.next_attempt < *until)
                        .map(|_| retry.next_attempt)
                })
                .min()
                .unwrap_or(sweep_at)
                .min(sweep_at);
            tokio::select! {
                _ = pool.readiness_wake.notify.notified() => {}
                _ = tokio::time::sleep_until(next) => {}
            }
        }
    });
}

/// Watch the named fence without taking its drive or cancellation token.
/// The caller retains one deadline across every refused open.
pub(crate) async fn wait_after_refusal(
    state: &ApiState,
    handle: &crate::operations::OperationHandle,
    refusal: Status,
    budget: Duration,
    deadline: &mut Option<(tokio::time::Instant, String)>,
) -> Result<(), Status> {
    let Some(ReadinessFence(id)) = ReadinessFence::from_status(&refusal) else {
        return Err(refusal);
    };
    if budget.is_zero() {
        return Err(refusal);
    }
    let (deadline, original_refusal) = deadline.get_or_insert_with(|| {
        (
            tokio::time::Instant::now() + budget,
            refusal.message().to_owned(),
        )
    });
    let deadline = *deadline;
    let index = state.index()?;
    loop {
        if state
            .readiness_shutdown
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(Status::unavailable(format!(
                "{original_refusal}; {}",
                SHUTDOWN_REASON
            )));
        }
        if handle.is_cancelled() {
            return Err(Status::cancelled(
                "operation cancelled during readiness wait",
            ));
        }
        let record = index
            .media_readiness_operation(id)
            .map_err(crate::status_from_state_error)?
            .ok_or_else(|| Status::not_found("readiness operation missing"))?;
        let latest = state.operations.latest(&id);
        let cancelled = record.cancel_source.as_deref() == Some("operator");
        let expired = tokio::time::Instant::now() >= deadline;
        let dispatch_failed = latest.as_ref().is_some_and(|status| {
            status.state == pb::OperationState::Failed as i32
                && status
                    .progress
                    .get("state")
                    .is_some_and(|state| state == "dispatch_failed")
        });
        if cancelled
            || expired
            || dispatch_failed
            || (!is_resumable(&record.state) && record.state != "ready")
        {
            let summary = latest
                .as_ref()
                .map(|s| s.error_summary.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(if expired {
                    "readiness wait deadline expired"
                } else if cancelled {
                    "readiness wait cancelled"
                } else {
                    "readiness wait stopped"
                });
            return Err(Status::failed_precondition(format!(
                "{}; readiness operation {id} state={} operation_state={:?}: {summary}",
                original_refusal,
                record.state,
                latest
                    .as_ref()
                    .and_then(|s| pb::OperationState::try_from(s.state).ok())
            )));
        }
        if record.state == "ready" {
            // The actor publishes READY before dropping its drive reservation.
            // Another owner can already hold that bay when an open needs a different bay.
            if !state.operations.is_live(&id)
                && !state
                    .drive_pool()?
                    .held_by_readiness_wait(&record.endpoint, id)
            {
                return Ok(());
            }
        }
        let sense = crate::startup_media_readiness::media_readiness_record_sense_label(&record)
            .unwrap_or_default();
        let sense_raw = match (record.last_sense_key, record.last_asc, record.last_ascq) {
            (Some(k), Some(a), Some(q)) => format!("{k:02x}/{a:02x}/{q:02x}"),
            _ => String::new(),
        };
        let sense_name = match (record.last_asc, record.last_ascq) {
            (Some(a), Some(q)) => {
                remanence_library::media_readiness_sense_name(a as u8, q as u8).unwrap_or("")
            }
            _ => "",
        };
        let (sense_key, name_key) = remanence_library::MEDIA_READINESS_SENSE_PROGRESS_KEYS;
        handle.publish_state(
            pb::OperationState::Running,
            &[
                ("phase", "readiness"),
                ("readiness_operation_id", &id.to_string()),
                ("state", &record.state),
                (sense_key, &sense_raw),
                (name_key, sense_name),
                ("detail", &sense),
            ],
        );
        tokio::time::sleep_until(std::cmp::min(
            deadline,
            tokio::time::Instant::now() + Duration::from_millis(250),
        ))
        .await;
    }
}

#[cfg(test)]
mod tests {
    /// Every recorded active state belongs to exactly one admission class.
    #[test]
    fn readiness_states_partition_active_records() {
        for state in [
            "planned",
            "pre_ready_loading",
            "media_initializing",
            "becoming_ready",
            "target_busy",
            "unit_attention",
            "aborted_unknown",
            "timeout_unknown",
            "transport_unknown",
            "terminal_error",
            "reservation_conflict",
        ] {
            assert_ne!(
                super::is_resumable(state),
                crate::startup_media_readiness::media_readiness_state_requires_release(state),
                "{state}"
            );
        }
        for state in ["ready", "released"] {
            assert!(!super::is_resumable(state));
            assert!(!crate::startup_media_readiness::media_readiness_state_requires_release(state));
        }
    }
}
