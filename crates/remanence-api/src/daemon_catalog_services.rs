//! gRPC implementations for daemon control and catalog discovery.

use std::pin::Pin;

use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::api_state::ApiState;
use crate::auth::{authorize_request, AuthPermission};
use crate::catalog_conversion::{
    audit_actor_fingerprint, blocking_status, catalog_unit_filter, catalog_unit_stream,
    catalog_unit_to_proto, decode_object_id, decode_required_idempotency, decode_text_id,
    decode_uuid_bytes, find_copy_object_for_key, find_object_for_key, list_entries_for_unit,
    manual_finalize_request_fingerprint, native_object_file_to_proto, native_object_stream,
    object_copy_to_proto, object_record_to_proto, operation_to_proto,
    reject_unimplemented_idempotency, tape_file_to_proto, tape_finalization_to_proto,
    tape_pool_to_proto, tape_to_proto, tape_to_proto_with_rollups,
};
use crate::catalog_request::{
    ensure_enumerate_objects_scope_is_all, ensure_enumerate_units_scope_is_all, ensure_unpaged,
};
use crate::pb;
use crate::startup_media_readiness::status_from_state_error;

type TapeInventoryItemStream =
    Pin<Box<dyn Stream<Item = Result<pb::TapeInventoryStreamItem, Status>> + Send + 'static>>;

/// Implementation of the process-level Daemon service.
#[derive(Clone)]
pub struct DaemonService {
    pub(crate) state: ApiState,
}

#[tonic::async_trait]
impl pb::daemon_server::Daemon for DaemonService {
    async fn health(&self, request: Request<()>) -> Result<Response<pb::HealthResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let quick_check = self
            .state
            .index()?
            .quick_check()
            .map_err(|err| Status::internal(err.to_string()))?;
        let status = if quick_check == "ok" {
            pb::health_response::Status::Healthy
        } else {
            pb::health_response::Status::Degraded
        };
        let mut components = std::collections::HashMap::new();
        components.insert("sqlite_index".to_string(), quick_check.clone());
        let component_status = if quick_check == "ok" {
            pb::component_health::Status::Healthy
        } else {
            pb::component_health::Status::Other
        };
        let component_health = vec![pb::ComponentHealth {
            component: "sqlite_index".to_string(),
            status: component_status as i32,
            other_status: if component_status == pb::component_health::Status::Other {
                quick_check.clone()
            } else {
                String::new()
            },
        }];
        Ok(Response::new(pb::HealthResponse {
            status: status as i32,
            components,
            detail: format!("sqlite quick_check={quick_check}"),
            component_health,
        }))
    }

    async fn version(&self, request: Request<()>) -> Result<Response<pb::VersionResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        Ok(Response::new(pb::VersionResponse {
            daemon_version: self.state.daemon_version.clone(),
            api_version: self.state.api_version.clone(),
            rust_target: self.state.rust_target.clone(),
        }))
    }

    async fn get_operation(
        &self,
        request: Request<pb::GetOperationRequest>,
    ) -> Result<Response<pb::OperationStatus>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let operation_uuid =
            decode_uuid_bytes(request.into_inner().operation_id.as_slice(), "operation_id")?;
        let operation_id = Uuid::from_bytes(operation_uuid).to_string();
        let operation = self
            .state
            .index()?
            .get_operation(operation_id.as_str())
            .map_err(|err| Status::internal(err.to_string()))?
            .ok_or_else(|| Status::not_found("operation not found"))?;
        Ok(Response::new(operation_to_proto(operation)?))
    }

    async fn list_operations(
        &self,
        request: Request<pb::ListOperationsRequest>,
    ) -> Result<Response<pb::ListOperationsResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let operations = self
            .state
            .index()?
            .list_operations()
            .map_err(|err| Status::internal(err.to_string()))?
            .into_iter()
            .filter(|record| {
                crate::operations::matches_filter(
                    record.operation_kind.as_str(),
                    record.state.as_str(),
                    record.started_at_utc.as_str(),
                    &request.filter,
                )
            })
            .map(operation_to_proto)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(pb::ListOperationsResponse {
            operations,
            next_page_token: None,
        }))
    }

    async fn cancel_operation(
        &self,
        request: Request<pb::CancelOperationRequest>,
    ) -> Result<Response<pb::CancelOperationResponse>, Status> {
        let actor = authorize_request(&request, AuthPermission::OperationControl)?;
        let request = request.into_inner();
        reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "CancelOperation")?;
        let operation_uuid = decode_uuid_bytes(request.operation_id.as_slice(), "operation_id")?;
        let operation_id = Uuid::from_bytes(operation_uuid);
        let resulting_state = self.state.operations.request_cancel(&operation_id)?;
        if matches!(
            resulting_state,
            pb::OperationState::Succeeded
                | pb::OperationState::Failed
                | pb::OperationState::Cancelled
        ) {
            return Ok(Response::new(pb::CancelOperationResponse {
                resulting_state: resulting_state as i32,
                detail: "operation is already terminal".to_string(),
            }));
        }
        self.state
            .record_cancel_requested(actor, operation_id, None, request.force)?;
        Ok(Response::new(pb::CancelOperationResponse {
            resulting_state: resulting_state as i32,
            detail: "cancellation requested".to_string(),
        }))
    }

    type WatchOperationStream = crate::operations::OperationStatusStream;

    async fn watch_operation(
        &self,
        request: Request<pb::GetOperationRequest>,
    ) -> Result<Response<Self::WatchOperationStream>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let operation_uuid =
            decode_uuid_bytes(request.into_inner().operation_id.as_slice(), "operation_id")?;
        let stream = self
            .state
            .operations
            .watch(&Uuid::from_bytes(operation_uuid))?;
        Ok(Response::new(stream))
    }
}

/// Implementation of the read-only Catalog service skeleton.
#[derive(Clone)]
pub struct CatalogService {
    pub(crate) state: ApiState,
}

#[tonic::async_trait]
impl pb::catalog_server::Catalog for CatalogService {
    async fn list_tapes(
        &self,
        request: Request<pb::ListTapesRequest>,
    ) -> Result<Response<pb::ListTapesResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        if !request.library_uuid.is_empty() {
            return Err(Status::unimplemented(
                "library-scoped tape listing is not wired in this slice",
            ));
        }
        ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let pool_id = request.pool_id.trim();
        let pool_id = if pool_id.is_empty() {
            None
        } else {
            Some(pool_id)
        };
        let kind = match request.kind.trim() {
            "" | "data" => remanence_state::TapeKindFilter::Data,
            "cleaning" => remanence_state::TapeKindFilter::Cleaning,
            "all" => remanence_state::TapeKindFilter::All,
            other => {
                return Err(Status::invalid_argument(format!(
                    "ListTapes kind must be empty, data, cleaning, or all, got {other:?}"
                )));
            }
        };
        let tapes = self
            .state
            .index()?
            .list_tapes(pool_id, kind)
            .map_err(status_from_state_error)?
            .into_iter()
            .map(tape_to_proto)
            .collect::<Vec<_>>();
        Ok(Response::new(pb::ListTapesResponse {
            tapes,
            next_page_token: None,
        }))
    }

    async fn list_tape_pools(
        &self,
        request: Request<pb::ListTapePoolsRequest>,
    ) -> Result<Response<pb::ListTapePoolsResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let pools = self
            .state
            .index()?
            .list_tape_pools()
            .map_err(|err| Status::internal(err.to_string()))?
            .into_iter()
            .map(tape_pool_to_proto)
            .collect::<Vec<_>>();
        Ok(Response::new(pb::ListTapePoolsResponse {
            pools,
            next_page_token: None,
        }))
    }

    async fn get_tape_pool(
        &self,
        request: Request<pb::GetTapePoolRequest>,
    ) -> Result<Response<pb::TapePool>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        let pool_id = request.pool_id.trim();
        if pool_id.is_empty() {
            return Err(Status::invalid_argument("pool_id must not be empty"));
        }
        let pool = self
            .state
            .index()?
            .get_tape_pool(pool_id)
            .map_err(status_from_state_error)?
            .ok_or_else(|| Status::not_found("tape pool not found"))?;
        Ok(Response::new(tape_pool_to_proto(pool)))
    }

    async fn get_tape(
        &self,
        request: Request<pb::GetTapeRequest>,
    ) -> Result<Response<pb::Tape>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        let tape_uuid = decode_uuid_bytes(request.tape_uuid.as_slice(), "tape_uuid")?;
        let index = self.state.index()?;
        let tape = index
            .get_tape(&tape_uuid)
            .map_err(|err| Status::internal(err.to_string()))?
            .ok_or_else(|| Status::not_found("tape not found"))?;
        let rollups = index
            .tape_drive_correlation_rollups(&tape_uuid)
            .map_err(status_from_state_error)?;
        Ok(Response::new(tape_to_proto_with_rollups(tape, rollups)))
    }

    type GetTapeInventoryStream = TapeInventoryItemStream;

    async fn get_tape_inventory(
        &self,
        request: Request<pb::TapeInventoryRequest>,
    ) -> Result<Response<Self::GetTapeInventoryStream>, Status> {
        authorize_request(&request, AuthPermission::ReadTape)?;
        let tape_uuid = decode_uuid_bytes(request.into_inner().tape_uuid.as_slice(), "tape_uuid")?;
        let inventory = crate::mount::tape_inventory(&self.state, tape_uuid).await?;
        Ok(Response::new(Box::pin(inventory)))
    }

    async fn verify_tape_index(
        &self,
        request: Request<pb::VerifyTapeIndexRequest>,
    ) -> Result<Response<pb::TapeIndexVerification>, Status> {
        authorize_request(&request, AuthPermission::ReadTape)?;
        let tape_uuid = decode_uuid_bytes(request.into_inner().tape_uuid.as_slice(), "tape_uuid")?;
        let verification = crate::mount::verify_tape_index(&self.state, tape_uuid).await?;
        Ok(Response::new(verification))
    }

    async fn list_tape_files(
        &self,
        request: Request<pb::ListTapeFilesRequest>,
    ) -> Result<Response<pb::ListTapeFilesResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let tape_uuid = decode_uuid_bytes(request.tape_uuid.as_slice(), "tape_uuid")?;
        let tape_files = self
            .state
            .index()?
            .list_tape_files(&tape_uuid)
            .map_err(|err| Status::internal(err.to_string()))?
            .into_iter()
            .map(tape_file_to_proto)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(pb::ListTapeFilesResponse {
            tape_files,
            next_page_token: None,
        }))
    }

    type EnumerateObjectsStream =
        Pin<Box<dyn Stream<Item = Result<pb::ObjectRecord, Status>> + Send + 'static>>;

    async fn enumerate_objects(
        &self,
        request: Request<pb::EnumerateObjectsRequest>,
    ) -> Result<Response<Self::EnumerateObjectsStream>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        ensure_enumerate_objects_scope_is_all(&request)?;
        if request.reconcile_from_tape {
            return Err(Status::unimplemented(
                "direct tape reconciliation is not wired in this slice",
            ));
        }
        Ok(Response::new(native_object_stream(self.state.index_path())))
    }

    async fn get_object(
        &self,
        request: Request<pb::GetObjectRequest>,
    ) -> Result<Response<pb::ObjectRecord>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        let object = find_object_for_key(&self.state, request.key)?
            .ok_or_else(|| Status::not_found("object not found"))?;
        Ok(Response::new(object_record_to_proto(object)?))
    }

    async fn find_object_copies(
        &self,
        request: Request<pb::FindObjectCopiesRequest>,
    ) -> Result<Response<pb::FindObjectCopiesResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        let object = find_copy_object_for_key(&self.state, request.key)?
            .ok_or_else(|| Status::not_found("object not found"))?;
        let copies = object
            .copies
            .iter()
            .map(object_copy_to_proto)
            .collect::<Vec<_>>();
        Ok(Response::new(pb::FindObjectCopiesResponse {
            object: Some(object_record_to_proto(object)?),
            copies,
        }))
    }

    async fn reconcile_tape(
        &self,
        request: Request<pb::ReconcileTapeRequest>,
    ) -> Result<Response<pb::OperationRef>, Status> {
        let actor = authorize_request(&request, AuthPermission::OperationControl)?;
        let request = request.into_inner();
        reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "ReconcileTape")?;
        let tape_uuid = decode_uuid_bytes(request.tape_uuid.as_slice(), "tape_uuid")?;
        let pool = self.state.drive_pool()?.clone();
        let library_serial =
            crate::mount::resolve_tape_library_serial(&self.state, &pool, &tape_uuid)?;
        let changer = pool.changer_tx(&library_serial)?;
        pool.reserve_library_exclusive(&library_serial)?;
        let operation_id = Uuid::new_v4();
        if let Err(err) = self.state.record_request_received(
            actor,
            operation_id,
            "reconcile_tape",
            &tape_uuid,
            None,
        ) {
            pool.release_library(&library_serial);
            return Err(err);
        }
        let handle = self
            .state
            .operations
            .register(operation_id, "reconcile_tape");
        match changer.try_send(crate::write_owner::ChangerCommand::Reconcile {
            tape_uuid,
            handle: handle.clone(),
        }) {
            Ok(()) => Ok(Response::new(pb::OperationRef {
                operation_id: operation_id.as_bytes().to_vec(),
            })),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let error = "drive-session owner is busy";
                pool.release_library(&library_serial);
                self.state
                    .record_operation_failed(operation_id, "reconcile_tape", error)?;
                handle.publish_failed(error, &[("phase", "dispatch")]);
                Err(Status::failed_precondition(error))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                let error = "drive-session owner is stopped";
                pool.release_library(&library_serial);
                self.state
                    .record_operation_failed(operation_id, "reconcile_tape", error)?;
                handle.publish_failed(error, &[("phase", "dispatch")]);
                Err(Status::unavailable(error))
            }
        }
    }

    #[allow(deprecated)]
    async fn finalize_tape(
        &self,
        request: Request<pb::FinalizeTapeRequest>,
    ) -> Result<Response<pb::TapeFinalization>, Status> {
        let actor = authorize_request(&request, AuthPermission::OperationControl)?;
        let request = request.into_inner();
        let tape_uuid = decode_uuid_bytes(request.tape_uuid.as_slice(), "tape_uuid")?;
        let expected_pool_id = request
            .expected_pool_id
            .map(|value| {
                if value.trim().is_empty() || value.trim() != value {
                    return Err(Status::invalid_argument(
                        "expected_pool_id must be non-empty canonical text without surrounding whitespace",
                    ));
                }
                Ok(value)
            })
            .transpose()?;
        if request.reason.trim().is_empty() {
            return Err(Status::invalid_argument(
                "reason must contain at least one non-whitespace character",
            ));
        }
        let idempotency_key =
            decode_required_idempotency(request.idempotency_key.as_ref(), "FinalizeTape")?;
        if idempotency_key.is_nil() {
            return Err(Status::invalid_argument(
                "FinalizeTape idempotency_key must not be the nil UUID",
            ));
        }
        let actor_fingerprint = audit_actor_fingerprint(&actor);
        let request_fingerprint = manual_finalize_request_fingerprint(
            tape_uuid,
            expected_pool_id.as_deref(),
            actor_fingerprint.as_str(),
            request.reason.as_bytes(),
        );
        let result = crate::mount::manual_finalize_tape(
            &self.state,
            crate::mount::ManualFinalizeTapeAdmission {
                candidate_operation_id: Uuid::new_v4(),
                actor,
                actor_fingerprint,
                idempotency_key,
                request_fingerprint,
                tape_uuid,
                expected_pool_id,
                reason: request.reason,
            },
        )
        .await?;
        let response = match result {
            crate::write_owner::ManualFinalizeTapeResult::Busy => pb::TapeFinalization {
                tape_uuid: tape_uuid.to_vec(),
                operation_id: Vec::new(),
                progress: pb::TapeFinalizationProgress::Unspecified as i32,
                completed_replicas: 0,
                replica_health: Vec::new(),
                replica_progress: Vec::new(),
                edition_digest: Vec::new(),
                layout_digest: Vec::new(),
                outcome: pb::TapeFinalizationOutcome::Busy as i32,
                trigger: "operator_close_out".to_string(),
                detail: "tape has an in-flight owner; no state or media motion occurred"
                    .to_string(),
            },
            crate::write_owner::ManualFinalizeTapeResult::Accepted(reply) => {
                tape_finalization_to_proto(tape_uuid, Some(reply.operation_id), reply.projection)
            }
        };
        Ok(Response::new(response))
    }

    async fn get_tape_finalization(
        &self,
        request: Request<pb::GetTapeFinalizationRequest>,
    ) -> Result<Response<pb::TapeFinalization>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let tape_uuid = decode_uuid_bytes(request.into_inner().tape_uuid.as_slice(), "tape_uuid")?;
        let index = self.state.index()?;
        let tape = index
            .get_tape(&tape_uuid)
            .map_err(status_from_state_error)?
            .ok_or_else(|| Status::not_found("tape not found"))?;
        let projection = tape
            .terminal_finalization
            .ok_or_else(|| Status::not_found("tape has no terminal-finalization authority"))?;
        Ok(Response::new(tape_finalization_to_proto(
            tape_uuid,
            projection.operation_id,
            projection,
        )))
    }

    async fn list_files_in_object(
        &self,
        request: Request<pb::ListFilesInObjectRequest>,
    ) -> Result<Response<pb::ListFilesInObjectResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let object_id = decode_object_id(request.object_id.as_slice())?;
        let index = self.state.index()?;
        index
            .get_native_object(object_id.as_str())
            .map_err(status_from_state_error)?
            .ok_or_else(|| Status::not_found("object not found"))?;
        let files = index
            .list_native_object_files(object_id.as_str())
            .map_err(status_from_state_error)?
            .into_iter()
            .map(native_object_file_to_proto)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Response::new(pb::ListFilesInObjectResponse {
            files,
            next_page_token: None,
        }))
    }

    async fn get_file(
        &self,
        request: Request<pb::GetFileRequest>,
    ) -> Result<Response<pb::FileRecord>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        let object_id = decode_object_id(request.object_id.as_slice())?;
        let index = self.state.index()?;
        index
            .get_native_object(object_id.as_str())
            .map_err(status_from_state_error)?
            .ok_or_else(|| Status::not_found("object not found"))?;
        let file = match request
            .key
            .ok_or_else(|| Status::invalid_argument("missing file lookup key"))?
        {
            pb::get_file_request::Key::FileId(file_id) => {
                let file_id = decode_text_id(file_id.as_slice(), "file_id")?;
                index
                    .get_native_object_file(object_id.as_str(), file_id.as_str())
                    .map_err(status_from_state_error)?
            }
            pb::get_file_request::Key::Path(path) => {
                if path.is_empty() {
                    return Err(Status::invalid_argument("path must not be empty"));
                }
                index
                    .list_native_object_files(object_id.as_str())
                    .map_err(status_from_state_error)?
                    .into_iter()
                    .find(|file| file.path == path)
            }
        }
        .ok_or_else(|| Status::not_found("object file not found"))?;
        Ok(Response::new(native_object_file_to_proto(file)?))
    }

    type EnumerateUnitsStream =
        Pin<Box<dyn Stream<Item = Result<pb::CatalogUnit, Status>> + Send + 'static>>;

    async fn enumerate_units(
        &self,
        request: Request<pb::EnumerateUnitsRequest>,
    ) -> Result<Response<Self::EnumerateUnitsStream>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        ensure_enumerate_units_scope_is_all(&request)?;
        if request.refresh_from_source {
            return Err(Status::unimplemented(
                "source refresh is not wired in this slice",
            ));
        }
        let filter = catalog_unit_filter(request.origin_filter);
        Ok(Response::new(catalog_unit_stream(
            self.state.index_path(),
            filter,
        )))
    }

    async fn get_catalog_unit(
        &self,
        request: Request<pb::GetCatalogUnitRequest>,
    ) -> Result<Response<pb::CatalogUnit>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let unit_id = decode_text_id(&request.into_inner().unit_id, "unit_id")?;
        let unit = self
            .state
            .index()?
            .get_catalog_unit(unit_id.as_str())
            .map_err(|err| Status::internal(err.to_string()))?
            .ok_or_else(|| Status::not_found("catalog unit not found"))?;
        Ok(Response::new(catalog_unit_to_proto(unit)?))
    }

    async fn list_entries_in_unit(
        &self,
        request: Request<pb::ListEntriesInUnitRequest>,
    ) -> Result<Response<pb::ListEntriesInUnitResponse>, Status> {
        authorize_request(&request, AuthPermission::Read)?;
        let request = request.into_inner();
        ensure_unpaged(request.page_token.as_ref(), request.page_size)?;
        let unit_id = decode_text_id(&request.unit_id, "unit_id")?;
        let unit = self
            .state
            .index()?
            .get_catalog_unit(unit_id.as_str())
            .map_err(|err| Status::internal(err.to_string()))?
            .ok_or_else(|| Status::not_found("catalog unit not found"))?;
        let foreign_formats = self.state.foreign_formats.clone();
        blocking_status(move || list_entries_for_unit(unit, &foreign_formats)).await
    }
}
