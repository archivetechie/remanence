//! Process-wide identity admission between catalog lookup and checkpoint.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use remanence_stream::normalize_archive_path;
use tonic::Status;
use uuid::Uuid;

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct WriteReplayKey {
    pool_id: String,
    caller_object_id: String,
}

#[derive(Debug, Default)]
struct WriteAdmissionState {
    replay_keys: HashSet<WriteReplayKey>,
    object_ids: HashSet<[u8; 16]>,
}

/// Process-wide admission authority shared by every drive actor.
///
/// SQLite remains the durable committed authority. These short-lived claims
/// cover only the interval from the last pre-motion catalog read through the
/// checkpoint projection, so two drive actors cannot both write an identity
/// that the catalog can commit only once.
#[derive(Clone, Debug, Default)]
pub(crate) struct WriteAdmissionCoordinator {
    state: Arc<Mutex<WriteAdmissionState>>,
}

impl WriteAdmissionCoordinator {
    pub(crate) fn reserve(
        &self,
        pool_id: &str,
        caller_object_id: &str,
        object_id: Option<[u8; 16]>,
    ) -> Result<WriteAdmissionReservation, Status> {
        let replay_key = (!caller_object_id.trim().is_empty()).then(|| WriteReplayKey {
            pool_id: pool_id.trim().to_string(),
            caller_object_id: caller_object_id.to_string(),
        });
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        if replay_key
            .as_ref()
            .is_some_and(|key| state.replay_keys.contains(key))
            || object_id.is_some_and(|id| state.object_ids.contains(&id))
        {
            return Err(Status::aborted(
                "an append with the same pool/caller replay key or canonical Object UUID is still awaiting checkpoint; retry after that checkpoint completes",
            ));
        }
        if let Some(key) = replay_key.as_ref() {
            state.replay_keys.insert(key.clone());
        }
        if let Some(id) = object_id {
            state.object_ids.insert(id);
        }
        drop(state);
        Ok(WriteAdmissionReservation {
            coordinator: self.clone(),
            replay_key,
            object_id,
            release_on_drop: true,
        })
    }
}

#[derive(Debug)]
pub(crate) struct WriteAdmissionReservation {
    coordinator: WriteAdmissionCoordinator,
    replay_key: Option<WriteReplayKey>,
    object_id: Option<[u8; 16]>,
    release_on_drop: bool,
}

impl WriteAdmissionReservation {
    /// Leave this identity in the coordinator until process restart.
    pub(crate) fn quarantine_until_restart(&mut self) {
        self.release_on_drop = false;
    }
}

impl Drop for WriteAdmissionReservation {
    fn drop(&mut self) {
        if !self.release_on_drop {
            return;
        }
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if let Some(key) = self.replay_key.as_ref() {
            state.replay_keys.remove(key);
        }
        if let Some(id) = self.object_id {
            state.object_ids.remove(&id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn validate_provisional_replay_guards(
    caller_object_id: &str,
    pending_input_kind: crate::WriteObjectInputKind,
    pending_archive_path: Option<&str>,
    pending_object_id: [u8; 16],
    pending_content_sha256: [u8; 32],
    requested_input_kind: crate::WriteObjectInputKind,
    requested_archive_path: &Path,
    expected_object_id: Option<[u8; 16]>,
    expected_content_sha256: Option<[u8; 32]>,
    requested_content_sha256: [u8; 32],
) -> Result<(), Status> {
    match (requested_input_kind, expected_object_id) {
        (crate::WriteObjectInputKind::LogicalFile, Some(_)) => {
            return Err(Status::invalid_argument(
                "expected_object_id is valid only for canonical plaintext REM object ingestion",
            ));
        }
        (crate::WriteObjectInputKind::CanonicalPlaintextRemObject, None) => {
            return Err(Status::invalid_argument(
                "canonical plaintext REM object ingestion requires expected_object_id",
            ));
        }
        _ => {}
    }
    if let Some(expected) = expected_content_sha256 {
        if expected != requested_content_sha256 {
            return Err(Status::failed_precondition(format!(
                "content SHA-256 guard mismatch inside checkpoint batch: expected={}, requested={}",
                crate::hex_encoding::bytes_to_hex(&expected),
                crate::hex_encoding::bytes_to_hex(&requested_content_sha256),
            )));
        }
    }
    if pending_input_kind != requested_input_kind {
        return Err(Status::already_exists(format!(
            "caller_object_id replay changed input kind inside checkpoint batch: caller_object_id={caller_object_id:?}"
        )));
    }
    if requested_input_kind == crate::WriteObjectInputKind::LogicalFile {
        let pending_archive_path = pending_archive_path.ok_or_else(|| {
            Status::internal("pending logical-file replay is missing its member-path projection")
        })?;
        let requested_archive_path = normalize_archive_path(requested_archive_path)
            .map_err(|error| Status::invalid_argument(error.to_string()))?;
        if pending_archive_path != requested_archive_path.as_str() {
            return Err(Status::already_exists(format!(
                "caller_object_id replay changed archive path inside checkpoint batch: caller_object_id={caller_object_id:?}, existing={pending_archive_path:?}, requested={requested_archive_path:?}"
            )));
        }
    }
    if let Some(expected) = expected_object_id {
        if expected != pending_object_id {
            return Err(Status::invalid_argument(format!(
                "canonical plaintext REM object replay identity mismatch inside checkpoint batch: committed={}, expected={}",
                Uuid::from_bytes(pending_object_id),
                Uuid::from_bytes(expected),
            )));
        }
    }
    if requested_content_sha256 != pending_content_sha256 {
        return Err(Status::already_exists(format!(
            "caller_object_id replay conflict inside checkpoint batch: caller_object_id={caller_object_id:?}, existing content_sha256={}, requested content_sha256={}",
            crate::hex_encoding::bytes_to_hex(&pending_content_sha256),
            crate::hex_encoding::bytes_to_hex(&requested_content_sha256),
        )));
    }
    Ok(())
}
