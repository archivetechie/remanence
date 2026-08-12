//! Read-session RPC implementation and tape-target resolution.

use std::pin::Pin;

use remanence_state::CatalogIndex;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::auth::{authorize_request, AuthPermission};
use crate::catalog_conversion::{
    decode_object_id, decode_text_id, decode_uuid_bytes, reject_unimplemented_idempotency,
};
use crate::live_status::CountingBytesStream;
use crate::pb;
use crate::startup_media_readiness::status_from_state_error;

pub(crate) type BytesChunkStream =
    Pin<Box<dyn Stream<Item = Result<pb::BytesChunk, Status>> + Send + 'static>>;

/// Implementation of the Layer 5 read-session service.
#[derive(Clone)]
pub struct ReadSessionApi {
    pub(crate) state: ApiState,
}

#[tonic::async_trait]
impl pb::read_session_service_server::ReadSessionService for ReadSessionApi {
    async fn open_read_session(
        &self,
        request: Request<pb::OpenReadSessionRequest>,
    ) -> Result<Response<pb::ReadSession>, Status> {
        authorize_request(&request, AuthPermission::ReadTape)?;
        let request = request.into_inner();
        reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "OpenReadSession")?;
        let target = select_read_target(&self.state, request.target)?;
        let resume_target = decode_read_resume_target(request.resume_target, target.tape_uuid())?;
        let session = crate::mount::open_read_session(&self.state, target, resume_target).await?;
        Ok(Response::new(session))
    }

    async fn close_read_session(
        &self,
        request: Request<pb::CloseReadSessionRequest>,
    ) -> Result<Response<pb::ReadSession>, Status> {
        authorize_request(&request, AuthPermission::ReadTape)?;
        let request = request.into_inner();
        reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "CloseReadSession")?;
        let session_id = decode_uuid_bytes(&request.session_id, "session_id")?;
        let session_id = Uuid::from_bytes(session_id);
        let session = crate::mount::close_read_session(&self.state, session_id).await?;
        Ok(Response::new(session))
    }

    async fn get_read_session(
        &self,
        request: Request<pb::GetReadSessionRequest>,
    ) -> Result<Response<pb::ReadSession>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let session_id = decode_uuid_bytes(&request.into_inner().session_id, "session_id")?;
        let session_id = Uuid::from_bytes(session_id);
        let session = crate::mount::get_read_session(&self.state, session_id).await?;
        Ok(Response::new(session))
    }

    type ReadObjectRangeStream = BytesChunkStream;

    async fn read_object_range(
        &self,
        request: Request<pb::ReadObjectRangeRequest>,
    ) -> Result<Response<Self::ReadObjectRangeStream>, Status> {
        authorize_request(&request, AuthPermission::ReadTape)?;
        let request = request.into_inner();
        let stream = if request.file_id.is_empty() {
            if request.start_byte == 0 && request.end_byte == 0 {
                self.dispatch_read_file(
                    request.session_id,
                    request.object_id,
                    request.file_id,
                    request.stream_chunk_bytes,
                )
                .await?
            } else {
                self.dispatch_read_object_range(
                    request.session_id,
                    request.object_id,
                    request.file_id,
                    request.start_byte,
                    request.end_byte,
                    request.stream_chunk_bytes,
                )
                .await?
            }
        } else {
            self.dispatch_read_object_range(
                request.session_id,
                request.object_id,
                request.file_id,
                request.start_byte,
                request.end_byte,
                request.stream_chunk_bytes,
            )
            .await?
        };
        Ok(Response::new(stream))
    }

    type ReadFileStream = BytesChunkStream;

    async fn read_file(
        &self,
        request: Request<pb::ReadFileRequest>,
    ) -> Result<Response<Self::ReadFileStream>, Status> {
        authorize_request(&request, AuthPermission::ReadTape)?;
        let request = request.into_inner();
        let stream = if request.file_id.is_empty() {
            self.dispatch_read_file(
                request.session_id,
                request.object_id,
                request.file_id,
                request.stream_chunk_bytes,
            )
            .await?
        } else {
            self.dispatch_read_object_range(
                request.session_id,
                request.object_id,
                request.file_id,
                0,
                0,
                request.stream_chunk_bytes,
            )
            .await?
        };
        Ok(Response::new(stream))
    }
}

pub(crate) fn decode_read_resume_target(
    target: Option<pb::ReadResumeTarget>,
    selected_tape_uuid: [u8; 16],
) -> Result<Option<crate::write_owner::ReadResumeTarget>, Status> {
    let Some(target) = target else {
        return Ok(None);
    };
    let tape_uuid = decode_uuid_bytes(&target.tape_uuid, "resume_target.tape_uuid")?;
    if tape_uuid != selected_tape_uuid {
        return Err(Status::invalid_argument(
            "resume_target.tape_uuid must match the read-session tape target",
        ));
    }
    let file_id = decode_text_id(&target.file_id, "resume_target.file_id")?;
    if file_id.is_empty() {
        return Err(Status::invalid_argument(
            "resume_target.file_id must identify a catalogued file",
        ));
    }
    Ok(Some(crate::write_owner::ReadResumeTarget {
        tape_uuid,
        object_id: decode_object_id(&target.object_id)?,
        file_id,
        file_boundary_byte_offset: target.file_boundary_byte_offset,
        expected_position_lba: target.expected_position_lba,
        prior_daemon_epoch: target.daemon_epoch,
    }))
}

impl ReadSessionApi {
    async fn dispatch_read_file(
        &self,
        session_id: Vec<u8>,
        object_id: Vec<u8>,
        file_id: Vec<u8>,
        stream_chunk_bytes: u32,
    ) -> Result<BytesChunkStream, Status> {
        let session_id = decode_uuid_bytes(&session_id, "session_id")?;
        let session_id = Uuid::from_bytes(session_id);
        let object_id = decode_object_id(&object_id)?;
        let (chunk_tx, chunk_rx) =
            crate::read_core::read_stream_channel(stream_chunk_bytes as usize);
        crate::mount::read_file(
            &self.state,
            session_id,
            object_id,
            file_id,
            stream_chunk_bytes,
            chunk_tx,
        )
        .await?;
        let state = self.state.clone();
        let drive_uuid = {
            let pool = state.drive_pool()?.clone();
            let mounted = pool.session(session_id)?;
            mounted.drive_uuid.clone()
        };
        Ok(Box::pin(CountingBytesStream {
            inner: Box::pin(chunk_rx),
            state,
            drive_uuid,
        }))
    }

    async fn dispatch_read_object_range(
        &self,
        session_id: Vec<u8>,
        object_id: Vec<u8>,
        file_id: Vec<u8>,
        start_byte: u64,
        end_byte: u64,
        stream_chunk_bytes: u32,
    ) -> Result<BytesChunkStream, Status> {
        let session_id = decode_uuid_bytes(&session_id, "session_id")?;
        let session_id = Uuid::from_bytes(session_id);
        let object_id = decode_object_id(&object_id)?;
        let file_id = decode_text_id(&file_id, "file_id")?;
        let (chunk_tx, chunk_rx) =
            crate::read_core::read_stream_channel(stream_chunk_bytes as usize);
        crate::mount::read_object_range(
            &self.state,
            crate::mount::ReadObjectRangeDispatch {
                session_id,
                object_id,
                file_id,
                start_byte,
                end_byte,
                stream_chunk_bytes,
            },
            chunk_tx,
        )
        .await?;
        let state = self.state.clone();
        let drive_uuid = {
            let pool = state.drive_pool()?.clone();
            let mounted = pool.session(session_id)?;
            mounted.drive_uuid.clone()
        };
        Ok(Box::pin(CountingBytesStream {
            inner: Box::pin(chunk_rx),
            state,
            drive_uuid,
        }))
    }
}

pub(crate) fn select_read_target(
    state: &ApiState,
    target: Option<pb::open_read_session_request::Target>,
) -> Result<crate::mount::ReadSessionTarget, Status> {
    let index = state.index()?;
    match target.ok_or_else(|| Status::invalid_argument("missing read-session target"))? {
        pb::open_read_session_request::Target::TapeTarget(target) => {
            if !target.mount_if_needed {
                return Err(Status::invalid_argument(
                    "tape-target read sessions require mount_if_needed=true in this slice",
                ));
            }
            let tape_uuid = decode_uuid_bytes(&target.tape_uuid, "tape_uuid")?;
            index
                .get_tape(&tape_uuid)
                .map_err(|err| Status::internal(err.to_string()))?
                .ok_or_else(|| Status::not_found("tape not found"))?;
            ensure_tape_matches_pool(&index, &tape_uuid, target.required_pool_id.as_str())?;
            Ok(crate::mount::ReadSessionTarget::Tape { tape_uuid })
        }
        pb::open_read_session_request::Target::DriveTarget(target) => {
            let bay = crate::library::narrow_element(
                target.drive_element_address,
                "drive_element_address",
            )?;
            let library_serial = resolve_read_target_library_serial(state, &target.library_uuid)?;
            if state.busy_drive_bays(&library_serial).contains(&bay) {
                return Err(Status::failed_precondition(format!(
                    "drive bay 0x{bay:04x} is busy"
                )));
            }
            let snapshot = state
                .current_library_snapshot()
                .ok_or_else(|| Status::not_found("library not found"))?;
            let library = snapshot
                .report
                .libraries
                .iter()
                .find(|library| library.serial == library_serial)
                .ok_or_else(|| Status::not_found("library not found"))?;
            let drive = library
                .drive_bays
                .iter()
                .find(|drive| drive.element_address == bay)
                .ok_or_else(|| Status::not_found(format!("drive bay 0x{bay:04x} not found")))?;
            if !drive.loaded {
                return Err(Status::failed_precondition(format!(
                    "drive bay 0x{bay:04x} is empty"
                )));
            }
            let barcode = drive.loaded_tape.as_deref().ok_or_else(|| {
                Status::failed_precondition(format!(
                    "drive bay 0x{bay:04x} tape identity cannot be proven: loaded media has no readable barcode"
                ))
            })?;
            let tape = index
                .get_tape_by_voltag(barcode)
                .map_err(status_from_state_error)?
                .ok_or_else(|| {
                    Status::failed_precondition(format!(
                        "drive bay 0x{bay:04x} tape identity cannot be proven: barcode {barcode} is not registered in the catalog"
                    ))
                })?;
            let tape_uuid = tape
                .tape_uuid
                .as_slice()
                .try_into()
                .map_err(|_| Status::internal("catalog tape UUID is not 16 bytes"))?;
            ensure_tape_matches_pool(&index, &tape_uuid, target.required_pool_id.as_str())?;
            Ok(crate::mount::ReadSessionTarget::LoadedDrive {
                tape_uuid,
                library_serial,
                bay,
            })
        }
    }
}

pub(crate) fn resolve_read_target_library_serial(
    state: &ApiState,
    requested_library_uuid: &[u8],
) -> Result<String, Status> {
    if requested_library_uuid.is_empty() {
        return state
            .default_library_serial
            .as_ref()
            .map(|serial| serial.as_str().to_string())
            .ok_or_else(|| {
                Status::invalid_argument(
                    "library_uuid is required when config does not name exactly one library",
                )
            });
    }
    let requested = decode_uuid_bytes(requested_library_uuid, "library_uuid")?;
    let snapshot = state
        .current_library_snapshot()
        .ok_or_else(|| Status::not_found("library not found"))?;
    let library_serial = snapshot
        .report
        .libraries
        .iter()
        .find(|library| crate::library::library_uuid(&library.serial) == requested)
        .map(|library| library.serial.clone())
        .ok_or_else(|| Status::not_found("library not found"))?;
    if !state.operates_library(&library_serial) {
        return Err(Status::failed_precondition(format!(
            "library {library_serial} is discovered but is not operated by this daemon"
        )));
    }
    Ok(library_serial)
}

pub(crate) fn ensure_tape_matches_pool(
    index: &CatalogIndex,
    tape_uuid: &[u8; 16],
    required_pool_id: &str,
) -> Result<(), Status> {
    let required_pool_id = required_pool_id.trim();
    if required_pool_id.is_empty() {
        return Ok(());
    }
    if !required_pool_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(Status::invalid_argument(format!(
            "tape pool id {required_pool_id:?} must use only ASCII letters, digits, '.', '_', '-', or ':'"
        )));
    }
    let membership = index
        .get_tape_pool_membership(tape_uuid)
        .map_err(status_from_state_error)?;
    match membership.as_deref() {
        Some(pool_id) if pool_id == required_pool_id => Ok(()),
        _ => Err(Status::failed_precondition(
            "tape is not assigned to the required pool",
        )),
    }
}
