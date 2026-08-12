//! Write-session RPC ingress, append streaming, and request validation.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::append_request::{
    append_spool_cap, archive_path_from_start, create_append_spool, ensure_same_session,
    expected_content_digest, finish_append_spool, overlap_append_eligible,
    validate_tape_target_shape, write_append_spool_chunk,
};
use crate::append_ring::{AppendRingControl, AppendRingProducer};
use crate::auth::{authorize_request, AuthPermission};
use crate::catalog_conversion::{decode_uuid_bytes, reject_unimplemented_idempotency};
use crate::hex_encoding::bytes_to_hex;
use crate::pb;
use crate::pool_write::{StreamedWriteSource, WriteObjectInputKind};

/// Implementation of the Layer 5 write-session service.
#[derive(Clone)]
pub struct WriteSessionApi {
    pub(crate) state: ApiState,
}

#[tonic::async_trait]
impl pb::write_session_service_server::WriteSessionService for WriteSessionApi {
    async fn open_write_session(
        &self,
        request: Request<pb::OpenWriteSessionRequest>,
    ) -> Result<Response<pb::WriteSession>, Status> {
        authorize_request(&request, AuthPermission::Write)?;
        let request = request.into_inner();
        reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "OpenWriteSession")?;
        if request.recover_session_id.is_some() {
            return Err(Status::unimplemented(
                "recover_session_id is not wired in this write-session slice",
            ));
        }
        // Absent asks for the daemon's default. A caller that supplied the
        // field and left it blank named no format, which is malformed.
        let body_format = match request.body_format {
            None => "rem-object-v1".to_string(),
            Some(format) if format.trim().is_empty() => {
                return Err(Status::invalid_argument(
                    "body_format must name a format when supplied; omit it for the default",
                ));
            }
            Some(format) => format.trim().to_string(),
        };
        if body_format != "rem-object-v1" {
            return Err(Status::unimplemented(format!(
                "write body format {body_format} is not wired in this slice"
            )));
        }
        let target = match request
            .target
            .ok_or_else(|| Status::invalid_argument("missing write-session target"))?
        {
            pb::open_write_session_request::Target::PoolTarget(target) => {
                if target.pool_id.trim().is_empty() {
                    return Err(Status::invalid_argument("pool_id must not be empty"));
                }
                if !target.mount_if_needed {
                    return Err(Status::invalid_argument(
                        "pool-target write sessions require mount_if_needed=true in this slice",
                    ));
                }
                let library_serial = self.library_serial_for_pool_target(&target)?;
                let session = crate::mount::open_write_session(
                    &self.state,
                    crate::mount::WriteSessionTarget::Pool {
                        pool_id: target.pool_id,
                    },
                    library_serial,
                )
                .await?;
                return Ok(Response::new(session));
            }
            pb::open_write_session_request::Target::TapeTarget(target) => target,
            pb::open_write_session_request::Target::DriveTarget(_) => {
                return Err(Status::unimplemented(
                    "drive-target write sessions are not wired in this slice",
                ));
            }
        };
        let (tape_uuid, required_pool_id) = validate_tape_target_shape(&target)?;
        let session = crate::mount::open_write_session(
            &self.state,
            crate::mount::WriteSessionTarget::PinnedTape {
                tape_uuid,
                required_pool_id,
            },
            None,
        )
        .await?;
        Ok(Response::new(session))
    }

    async fn append_object(
        &self,
        request: Request<tonic::Streaming<pb::AppendObjectMessage>>,
    ) -> Result<Response<pb::ObjectRecord>, Status> {
        let spool_dir = self.spool_dir_for_log();
        let result = async {
            authorize_request(&request, AuthPermission::Write)?;
            self.append_object_stream(request.into_inner()).await
        }
        .await;
        if let Err(status) = &result {
            log_append_object_failure(spool_dir.as_str(), status);
        }
        result
    }

    async fn checkpoint_session(
        &self,
        request: Request<pb::CheckpointSessionRequest>,
    ) -> Result<Response<pb::CheckpointSessionResponse>, Status> {
        authorize_request(&request, AuthPermission::Write)?;
        let request = request.into_inner();
        reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "CheckpointSession")?;
        let session_id = decode_uuid_bytes(&request.session_id, "session_id")?;
        let session_id = Uuid::from_bytes(session_id);
        let checkpoint = crate::mount::checkpoint_write_session(
            &self.state,
            session_id,
            crate::write_owner::CheckpointTrigger::Explicit,
        )
        .await?;
        let committed_copies = checkpoint
            .committed_objects
            .iter()
            .flat_map(|object| object.copies.iter().cloned())
            .collect();
        Ok(Response::new(pb::CheckpointSessionResponse {
            session: Some(checkpoint.session),
            committed_objects: checkpoint.committed_objects,
            committed_copies,
        }))
    }

    async fn close_write_session(
        &self,
        request: Request<pb::CloseWriteSessionRequest>,
    ) -> Result<Response<pb::WriteSession>, Status> {
        authorize_request(&request, AuthPermission::Write)?;
        let request = request.into_inner();
        reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "CloseWriteSession")?;
        let session_id = decode_uuid_bytes(&request.session_id, "session_id")?;
        let session_id = Uuid::from_bytes(session_id);
        let session = crate::mount::close_write_session(&self.state, session_id).await?;
        Ok(Response::new(session))
    }

    async fn abort_write_session(
        &self,
        request: Request<pb::AbortWriteSessionRequest>,
    ) -> Result<Response<pb::WriteSession>, Status> {
        authorize_request(&request, AuthPermission::Write)?;
        let request = request.into_inner();
        reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "AbortWriteSession")?;
        let session_id = decode_uuid_bytes(&request.session_id, "session_id")?;
        let session_id = Uuid::from_bytes(session_id);
        let session =
            crate::mount::abort_write_session(&self.state, session_id, request.reason).await?;
        Ok(Response::new(session))
    }

    async fn get_write_session(
        &self,
        request: Request<pb::GetWriteSessionRequest>,
    ) -> Result<Response<pb::WriteSession>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let session_id = decode_uuid_bytes(&request.into_inner().session_id, "session_id")?;
        let session_id = Uuid::from_bytes(session_id);
        let session = crate::mount::get_write_session(&self.state, session_id).await?;
        Ok(Response::new(session))
    }
}

impl WriteSessionApi {
    fn spool_dir_for_log(&self) -> String {
        self.state
            .spool_dir
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(unconfigured)".to_string())
    }

    #[cfg(test)]
    pub(crate) async fn append_object_stream_logged<S>(
        &self,
        stream: S,
    ) -> Result<Response<pb::ObjectRecord>, Status>
    where
        S: Stream<Item = Result<pb::AppendObjectMessage, Status>> + Unpin + Send + 'static,
    {
        let spool_dir = self.spool_dir_for_log();
        let result = self.append_object_stream(stream).await;
        if let Err(status) = &result {
            log_append_object_failure(spool_dir.as_str(), status);
        }
        result
    }

    pub(crate) async fn append_object_stream<S>(
        &self,
        mut stream: S,
    ) -> Result<Response<pb::ObjectRecord>, Status>
    where
        S: Stream<Item = Result<pb::AppendObjectMessage, Status>> + Unpin + Send + 'static,
    {
        let first = next_append_message(&mut stream)
            .await?
            .ok_or_else(|| Status::invalid_argument("append stream is empty"))?;
        let (
            session_id,
            caller_object_id,
            declared_size_bytes,
            start_digest,
            archive_path,
            expected_object_id,
            input_kind,
            logical_start,
        ) = match first.payload {
            Some(pb::append_object_message::Payload::Start(start)) => {
                if start.body_format_manifest.is_some() {
                    return Err(Status::unimplemented(
                        "body_format_manifest is not wired in this write-session slice",
                    ));
                }
                let session_id =
                    Uuid::from_bytes(decode_uuid_bytes(&start.session_id, "session_id")?);
                let start_digest = expected_content_digest(
                    start.expected_content_sha256.as_deref(),
                    start.expected_content_digest.as_ref(),
                )?;
                let archive_path = archive_path_from_start(&start);
                (
                    session_id,
                    start.caller_object_id.clone(),
                    start.declared_size_bytes,
                    start_digest,
                    archive_path,
                    None,
                    WriteObjectInputKind::LogicalFile,
                    Some(start),
                )
            }
            Some(pb::append_object_message::Payload::CanonicalStart(start)) => {
                if start.declared_size_bytes == 0 {
                    return Err(Status::invalid_argument(
                        "canonical REM object declared_size_bytes must be nonzero",
                    ));
                }
                if start.source_replay_capability
                    != pb::SourceReplayCapability::ReplayFromStart as i32
                {
                    return Err(Status::failed_precondition(
                        "canonical REM object source must be replayable from byte zero",
                    ));
                }
                if start.expected_caller_object_id.trim().is_empty() {
                    return Err(Status::invalid_argument(
                        "canonical REM object expected_caller_object_id must not be empty",
                    ));
                }
                let session_id =
                    Uuid::from_bytes(decode_uuid_bytes(&start.session_id, "session_id")?);
                let expected_object_id =
                    decode_uuid_bytes(&start.expected_object_id, "expected_object_id")?;
                let object_uuid = Uuid::from_bytes(expected_object_id);
                if object_uuid.is_nil() {
                    return Err(Status::invalid_argument(
                        "canonical REM object expected_object_id must not be nil",
                    ));
                }
                let start_digest =
                    expected_content_digest(None, start.expected_plaintext_digest.as_ref())?
                        .ok_or_else(|| {
                            Status::invalid_argument(
                                "canonical REM object expected_plaintext_digest is required",
                            )
                        })?;
                (
                    session_id,
                    start.expected_caller_object_id,
                    Some(start.declared_size_bytes),
                    Some(start_digest),
                    PathBuf::new(),
                    Some(expected_object_id),
                    WriteObjectInputKind::CanonicalPlaintextRemObject,
                    None,
                )
            }
            _ => {
                return Err(Status::invalid_argument(
                    "first AppendObject message must be Start or CanonicalStart",
                ));
            }
        };
        if let Some(start) = logical_start {
            let overlap_eligible = overlap_append_eligible(
                self.state.append_staging_mode,
                &start,
                start_digest.as_ref(),
            );
            if overlap_eligible {
                return self
                    .append_object_overlap(
                        stream,
                        start,
                        session_id,
                        start_digest.expect("overlap eligibility requires a start digest"),
                    )
                    .await;
            }
        }
        let cap = append_spool_cap(declared_size_bytes);
        self.state.validate_spool_budget(cap)?;
        let mut spool = create_append_spool(self.state.spool_dir()?.to_path_buf(), cap).await?;
        let mut spool_permits = Vec::new();
        let mut finish = None;
        let spool_started = Instant::now();
        let mut spool_bytes = 0u64;
        let mut spool_chunks = 0u64;
        while let Some(message) = next_append_message(&mut stream).await? {
            match message.payload.ok_or_else(|| {
                Status::invalid_argument("append stream message is missing payload")
            })? {
                pb::append_object_message::Payload::Chunk(chunk) => {
                    if finish.is_some() {
                        let _ = fs::remove_file(spool.path());
                        return Err(Status::invalid_argument(
                            "append stream has chunk after finish",
                        ));
                    }
                    if let Err(err) = ensure_same_session(&chunk.session_id, session_id) {
                        let _ = fs::remove_file(spool.path());
                        return Err(err);
                    }
                    let chunk_len = chunk.data.len() as u64;
                    let permit = self.state.reserve_io_memory(chunk_len)?;
                    spool = write_append_spool_chunk(spool, chunk.data).await?;
                    spool_permits.push(permit);
                    spool_bytes = spool_bytes.saturating_add(chunk_len);
                    spool_chunks = spool_chunks.saturating_add(1);
                }
                pb::append_object_message::Payload::Finish(next_finish) => {
                    if finish.is_some() {
                        let _ = fs::remove_file(spool.path());
                        return Err(Status::invalid_argument(
                            "append stream has more than one finish message",
                        ));
                    }
                    if let Err(err) = ensure_same_session(&next_finish.session_id, session_id) {
                        let _ = fs::remove_file(spool.path());
                        return Err(err);
                    }
                    finish = Some(next_finish);
                }
                pb::append_object_message::Payload::Start(_) => {
                    let _ = fs::remove_file(spool.path());
                    return Err(Status::invalid_argument(
                        "append stream has more than one start message",
                    ));
                }
                pb::append_object_message::Payload::CanonicalStart(_) => {
                    let _ = fs::remove_file(spool.path());
                    return Err(Status::invalid_argument(
                        "append stream has more than one start message",
                    ));
                }
            }
        }
        let finish =
            finish.ok_or_else(|| Status::invalid_argument("append stream must end with Finish"))?;
        let finish_digest = expected_content_digest(
            finish.expected_content_sha256.as_deref(),
            finish.expected_content_digest.as_ref(),
        )?;
        if start_digest.is_some() && finish_digest.is_some() && start_digest != finish_digest {
            let _ = fs::remove_file(spool.path());
            return Err(Status::invalid_argument(
                "Start and Finish expected_content_sha256 values disagree",
            ));
        }
        if input_kind == WriteObjectInputKind::CanonicalPlaintextRemObject
            && Some(spool_bytes) != declared_size_bytes
        {
            let _ = fs::remove_file(spool.path());
            return Err(Status::invalid_argument(format!(
                "canonical REM object streamed {spool_bytes} bytes, expected {}",
                declared_size_bytes.expect("canonical start requires declared size")
            )));
        }
        let expected_content_sha256 = start_digest.or(finish_digest);
        let spool_path = finish_append_spool(spool).await?;
        let spool_elapsed = spool_started.elapsed();
        tracing::info!(
            target: "remanence_write_diag",
            phase = "spool",
            session_id = %session_id,
            payload_bytes = spool_bytes,
            chunks = spool_chunks,
            declared_size_bytes,
            elapsed_ms = crate::diagnostics::duration_ms(spool_elapsed),
            throughput_mib_s = crate::diagnostics::mib_per_s(spool_bytes, spool_elapsed),
            "remanence_write_diag",
        );
        let append_finish_started = Instant::now();
        let record = match crate::mount::append_finish(
            &self.state,
            session_id,
            crate::mount::AppendFinishRequest {
                spool_path: spool_path.clone(),
                archive_path,
                caller_object_id,
                expected_content_sha256,
                expected_object_id,
                input_kind,
            },
        )
        .await
        {
            Ok(record) => record,
            Err(err) => {
                let _ = fs::remove_file(spool_path);
                return Err(err);
            }
        };
        let append_finish_elapsed = append_finish_started.elapsed();
        tracing::info!(
            target: "remanence_write_diag",
            phase = "append_finish",
            session_id = %session_id,
            payload_bytes = spool_bytes,
            elapsed_ms = crate::diagnostics::duration_ms(append_finish_elapsed),
            throughput_mib_s = crate::diagnostics::mib_per_s(spool_bytes, append_finish_elapsed),
            "remanence_write_diag",
        );
        Ok(Response::new(record))
    }

    async fn append_object_overlap<S>(
        &self,
        stream: S,
        start: pb::AppendObjectStart,
        session_id: Uuid,
        start_digest: [u8; 32],
    ) -> Result<Response<pb::ObjectRecord>, Status>
    where
        S: Stream<Item = Result<pb::AppendObjectMessage, Status>> + Unpin + Send + 'static,
    {
        // Overlap holds the stream to the declared size byte for byte, so it is
        // only ever entered with one; overlap_append_eligible is the guard.
        let declared_size_bytes = start
            .declared_size_bytes
            .expect("overlap eligibility requires a declared size");
        let (producer, consumer, control) = crate::append_ring::create_append_ring(
            &self.state.io_memory,
            self.state.append_ring_bytes,
            self.state.append_ring_high_pct,
            self.state.append_ring_low_pct,
            declared_size_bytes,
        )?;
        let receive_control = Arc::clone(&control);
        let receive_task = tokio::spawn(receive_overlap_messages(
            stream,
            producer,
            session_id,
            declared_size_bytes,
            start_digest,
            receive_control,
        ));
        if let Err(status) = control.wait_for_prefill().await {
            let receive = receive_task.await.map_err(|err| {
                Status::internal(format!("overlap receive task failed before prefill: {err}"))
            })?;
            return Err(receive.err().unwrap_or(status));
        }
        tracing::info!(
            target: "remanence_write_diag",
            phase = "overlap_prefill",
            session_id = %session_id,
            ring_occupancy_bytes = control.occupancy_bytes(),
            ring_peak_occupancy_bytes = control.peak_occupancy_bytes(),
            ring_capacity_bytes = control.capacity_bytes(),
            ring_high_bytes = control.high_bytes(),
            declared_size_bytes,
            client_live = true,
            "remanence_write_diag",
        );

        let source = StreamedWriteSource::new(
            consumer,
            declared_size_bytes,
            start_digest,
            Arc::clone(&control),
        );
        let append_started = Instant::now();
        let append = crate::mount::append_streamed(
            &self.state,
            session_id,
            source,
            archive_path_from_start(&start),
            start.caller_object_id,
            start_digest,
        )
        .await;
        let append = match append {
            Ok(mut outcome)
                if outcome
                    .record
                    .append_commit_info
                    .as_ref()
                    .is_some_and(|info| {
                        info.durability == pb::AppendDurability::Written as i32
                    }) =>
            {
                let object_id = outcome.record.object_id.clone();
                let checkpoint = crate::mount::checkpoint_write_session(
                    &self.state,
                    session_id,
                    crate::write_owner::CheckpointTrigger::Explicit,
                )
                .await;
                match checkpoint {
                    Ok(checkpoint) => {
                        outcome.record = checkpoint
                            .committed_objects
                            .into_iter()
                            .find(|record| record.object_id == object_id)
                            .ok_or_else(|| {
                                Status::internal(
                                    "overlap checkpoint omitted the just-written object",
                                )
                            })?;
                        Ok(outcome)
                    }
                    Err(err) => Err(err),
                }
            }
            other => other,
        };
        match append {
            Ok(outcome) if outcome.replay => {
                receive_task.abort();
                let _ = receive_task.await;
                Ok(Response::new(outcome.record))
            }
            Ok(outcome) => {
                let receive = receive_task.await.map_err(|err| {
                    Status::internal(format!("overlap receive task failed: {err}"))
                })??;
                tracing::info!(
                    target: "remanence_write_diag",
                    phase = "overlap_complete",
                    session_id = %session_id,
                    payload_bytes = receive.bytes,
                    chunks = receive.chunks,
                    ring_peak_occupancy_bytes = control.peak_occupancy_bytes(),
                    elapsed_ms = crate::diagnostics::duration_ms(append_started.elapsed()),
                    throughput_mib_s = crate::diagnostics::mib_per_s(
                        receive.bytes,
                        append_started.elapsed()
                    ),
                    "remanence_write_diag",
                );
                Ok(Response::new(outcome.record))
            }
            Err(actor_status) => {
                if receive_task.is_finished() {
                    if let Ok(Err(receive_status)) = receive_task.await {
                        return Err(receive_status);
                    }
                } else {
                    receive_task.abort();
                    let _ = receive_task.await;
                }
                Err(actor_status)
            }
        }
    }

    pub(crate) fn library_serial_for_pool_target(
        &self,
        target: &pb::TapePoolTarget,
    ) -> Result<Option<String>, Status> {
        let serial = if target.library_uuid.is_empty() {
            self.state
                .default_library_serial
                .as_ref()
                .map(|serial| serial.as_str().to_string())
        } else {
            let requested = decode_uuid_bytes(target.library_uuid.as_slice(), "library_uuid")?;
            let snapshot = self
                .state
                .current_library_snapshot()
                .ok_or_else(|| Status::not_found("library not found"))?;
            let serial = snapshot
                .report
                .libraries
                .iter()
                .find(|library| crate::library::library_uuid(&library.serial) == requested)
                .map(|library| library.serial.clone())
                .ok_or_else(|| Status::not_found("library not found"))?;
            if !self.state.operates_library(&serial) {
                return Err(Status::failed_precondition(format!(
                    "library {serial} is discovered but is not operated by this daemon"
                )));
            }
            Some(serial)
        };
        if serial
            .as_deref()
            .is_some_and(|serial| serial.trim().is_empty())
        {
            return Err(Status::invalid_argument("library serial must not be empty"));
        }
        Ok(serial.map(|serial| serial.trim().to_string()))
    }
}

pub(crate) async fn next_append_message<S>(
    stream: &mut S,
) -> Result<Option<pb::AppendObjectMessage>, Status>
where
    S: Stream<Item = Result<pb::AppendObjectMessage, Status>> + Unpin,
{
    match stream.next().await {
        Some(Ok(message)) => Ok(Some(message)),
        Some(Err(err)) => Err(Status::invalid_argument(format!(
            "append stream failed: {err}"
        ))),
        None => Ok(None),
    }
}

#[derive(Debug)]
pub(crate) struct OverlapReceiveReport {
    pub(crate) bytes: u64,
    pub(crate) chunks: u64,
}

/// Receive and hash one overlap body. The final slab is withheld until the
/// exact byte count, stream shape, receiver digest, and Finish digest pass.
pub(crate) async fn receive_overlap_messages<S>(
    mut stream: S,
    mut producer: AppendRingProducer,
    session_id: Uuid,
    declared_size_bytes: u64,
    start_digest: [u8; 32],
    control: Arc<AppendRingControl>,
) -> Result<OverlapReceiveReport, Status>
where
    S: Stream<Item = Result<pb::AppendObjectMessage, Status>> + Unpin,
{
    let receive_started = Instant::now();
    let mut sample_started = receive_started;
    let mut received_bytes = 0u64;
    let mut chunks = 0u64;
    let mut hasher = Sha256::new();
    let mut finish = None;
    let receive_result = async {
        while let Some(message) = next_append_message(&mut stream).await? {
            match message.payload.ok_or_else(|| {
                Status::invalid_argument("append stream message is missing payload")
            })? {
                pb::append_object_message::Payload::Chunk(chunk) => {
                    if finish.is_some() {
                        return Err(Status::invalid_argument(
                            "append stream has chunk after finish",
                        ));
                    }
                    ensure_same_session(&chunk.session_id, session_id)?;
                    let chunk_len = chunk.data.len() as u64;
                    let next = received_bytes.checked_add(chunk_len).ok_or_else(|| {
                        Status::invalid_argument("append received byte count overflows u64")
                    })?;
                    if next > declared_size_bytes {
                        return Err(Status::invalid_argument(format!(
                            "append body exceeds declared_size_bytes {declared_size_bytes}"
                        )));
                    }
                    hasher.update(&chunk.data);
                    producer.push(&chunk.data).await?;
                    received_bytes = next;
                    chunks = chunks.saturating_add(1);
                    if sample_started.elapsed() >= Duration::from_secs(1) {
                        crate::append_ring::log_ring_sample(
                            session_id,
                            &control,
                            received_bytes,
                            receive_started,
                            sample_started.elapsed(),
                        );
                        sample_started = Instant::now();
                    }
                }
                pb::append_object_message::Payload::Finish(next_finish) => {
                    if finish.is_some() {
                        return Err(Status::invalid_argument(
                            "append stream has more than one finish message",
                        ));
                    }
                    ensure_same_session(&next_finish.session_id, session_id)?;
                    finish = Some(next_finish);
                }
                pb::append_object_message::Payload::Start(_) => {
                    return Err(Status::invalid_argument(
                        "append stream has more than one start message",
                    ));
                }
                pb::append_object_message::Payload::CanonicalStart(_) => {
                    return Err(Status::invalid_argument(
                        "append stream has more than one start message",
                    ));
                }
            }
        }
        let finish = finish
            .ok_or_else(|| Status::invalid_argument("append stream must end with Finish"))?;
        if received_bytes != declared_size_bytes {
            return Err(Status::invalid_argument(format!(
                "append received {received_bytes} bytes but declared_size_bytes is {declared_size_bytes}"
            )));
        }
        let finish_digest = expected_content_digest(
            finish.expected_content_sha256.as_deref(),
            finish.expected_content_digest.as_ref(),
        )?;
        if finish_digest.is_some_and(|digest| digest != start_digest) {
            return Err(Status::invalid_argument(
                "Start and Finish expected_content_sha256 values disagree",
            ));
        }
        let actual = hasher.finalize();
        if actual.as_slice() != start_digest {
            return Err(Status::invalid_argument(format!(
                "append payload SHA-256 {} does not match Start expected_content_sha256 {}",
                bytes_to_hex(actual.as_slice()),
                bytes_to_hex(&start_digest)
            )));
        }
        Ok(OverlapReceiveReport {
            bytes: received_bytes,
            chunks,
        })
    }
    .await;

    match receive_result {
        Ok(report) => {
            producer.finish().await?;
            crate::append_ring::log_ring_sample(
                session_id,
                &control,
                report.bytes,
                receive_started,
                sample_started.elapsed(),
            );
            Ok(report)
        }
        Err(status) => {
            producer.abort(&status).await;
            Err(status)
        }
    }
}

pub(crate) fn log_append_object_failure(spool_dir: &str, status: &Status) {
    let code = status.code();
    let message = status.message();
    if matches!(
        code,
        tonic::Code::Internal
            | tonic::Code::Unknown
            | tonic::Code::Unavailable
            | tonic::Code::DataLoss
    ) {
        tracing::error!(
            target: "remanence_api::append_object",
            "append_object failed spool_dir={} status_code={:?} status_message={}",
            spool_dir,
            code,
            message,
        );
    } else {
        tracing::warn!(
            target: "remanence_api::append_object",
            "append_object failed spool_dir={} status_code={:?} status_message={}",
            spool_dir,
            code,
            message,
        );
    }
}
