//! Layer 5 gRPC service skeleton over the local Remanence state index.
//!
//! This crate owns the generated `proto/layer5.proto` bindings and the first
//! in-process service implementations. Daemon/catalog methods are backed by
//! `remanence-state::CatalogIndex`; read and write sessions dispatch to a
//! hardware-backed changer/drive actor pool when the daemon enables writes.

use std::path::PathBuf;
use std::time::Duration;

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::io;
#[cfg(test)]
use std::path::Path;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::sync::RwLock;
#[cfg(test)]
use std::time::Instant;

#[cfg(test)]
use remanence_format_driver::{
    ArchiveGapCause, ArchiveGapRange, EntryCatalogSink, EntryKind, ForeignFormatRegistry,
    FormatError, NormalizedEntry, ScanIntegrityBasis, SourceRequirement,
};
#[cfg(test)]
use remanence_state::{
    AuditEventRecord, CatalogIndex, FileAuditLog, MediaReadinessOperationRecord,
    NativeObjectCopyRecord, NativeObjectFileRecord, NativeObjectRecord, StateError, StateHandle,
    TapePoolConfig, TapeRecord,
};
#[cfg(test)]
use time::OffsetDateTime;
use tonic::transport::{Channel, Endpoint, Uri};
#[cfg(test)]
use tonic::Request;
#[cfg(test)]
use uuid::Uuid;

pub mod pb {
    tonic::include_proto!("remanence.api.v1");
}

/// Vendored `google.rpc` rich-error detail types (see `proto/google/rpc/`),
/// carried in the `grpc-status-details-bin` trailer of malformed-request
/// errors so a caller can recover the offending target index structurally.
pub mod pb_rpc {
    tonic::include_proto!("google.rpc");
}

/// Default maximum bytes admitted into one append spool reservation.
pub const APPEND_SPOOL_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;

/// Connect a gRPC channel to a Unix-socket daemon (Layer 5 dev transport).
/// The URI authority is a placeholder ignored by the custom connector.
pub async fn connect_unix(socket_path: PathBuf) -> Result<Channel, tonic::transport::Error> {
    Endpoint::try_from("http://[::1]:50051")?
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_timeout(Duration::from_secs(20))
        .keep_alive_while_idle(true)
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = socket_path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
}

pub use remanence_parity::ParityConfig;

mod api_state;
mod append_request;
mod append_ring;
mod append_spool;
mod audit_projection;
mod audit_query_service;
mod auth;
mod calibration;
mod catalog_conversion;
mod catalog_request;
mod daemon_catalog_services;
mod diagnostics;
mod direct_replay_fault;
mod drive_collection;
mod drive_mode;
mod drive_pool;
mod hex_encoding;
mod io_memory;
mod library;
mod live_status;
mod mount;
mod object_fault;
mod operations;
mod pool_selection;
mod pool_write;
pub mod read_core;
mod read_plan;
mod read_session_service;
mod startup_checkpoint;
mod startup_guard;
mod startup_media_readiness;
mod tape_init;
mod terminal_fault;
mod write_admission;
mod write_owner;
mod write_session_ingress;

pub use api_state::ApiState;
pub(crate) use audit_projection::FINALIZE_TAPE_OPERATION_KIND;
pub use audit_query_service::AuditApi;
#[cfg(test)]
use auth::{
    actor_from_request, parse_certificate_role_attribute, parse_client_role,
    role_from_certificate_subject, ClientRole,
};
pub(crate) use catalog_conversion::{append_mode_for_tape_file_number, timestamp_from_rfc3339};
pub use daemon_catalog_services::{CatalogService, DaemonService};
pub(crate) use hex_encoding::bytes_to_hex;
pub use library::LibraryServiceApi;
pub(crate) use live_status::{DriveByteCounters, LibrarySnapshot};
pub use mount::{load_tape_by_uuid, LoadByUuidError};
pub use pool_write::{
    build_tape_bootstrap, can_read, can_write, check_writability_preconditions,
    lto_generation_from_drive_product, lto_generation_from_voltag, raw_capacity_bytes,
    replay_committed_pool_write_from_state, seal_decision_after_write, select_tape_in_pool,
    select_tape_in_pool_for_write_session, verify_tape_identity, write_tape_bootstrap,
    write_to_selected_drive_checkpointed, LtoGen, ObjectWriteMediaError, PoolWriteError,
    PoolWriteObjectCopyRecord, PoolWriteObjectRecord, PoolWriteRepresentation, PoolWriteResources,
    PoolWriteResult, SelectTapeError, SelectedTape, StreamedWriteSource, TapeIdentityError,
    TapePositionAfterWrite, TapeSealReason, TapeUuid, WritabilityError, WriteObjectInputKind,
    WriteObjectSource, WriteObjectToPoolRequest,
};
#[cfg(test)]
pub use pool_write::{write_object_to_pool, write_to_selected_tape};
pub use read_plan::ReadPlanApi;
pub use read_session_service::ReadSessionApi;
pub use remanence_library::{resolve_load_target, LoadError, LoadPlan};
pub use startup_checkpoint::reconcile_checkpoint_journal_projections;
pub(crate) use startup_guard::tape_io_runtime_config;
pub use startup_media_readiness::reconcile_media_readiness_on_startup;
pub(crate) use startup_media_readiness::status_from_state_error;
pub use tape_init::{
    classify_bootstrap_adoption_from_source, classify_bot_bytes, classify_bot_from_source,
    classify_bot_identity_bytes, decide_tape_init, has_canonical_adoption_geometry,
    maybe_write_tape_init_bootstrap, probe_bot_identity_from_source,
    project_tape_init_catalog_inputs, sniff, BarcodeLifecycleState, BootstrapAdoptionProjection,
    BootstrapTailClassification, BotClassification, BotIdentityClassification, BotInitProjection,
    CatalogBarcodeRelation, CatalogRowDisposition, CatalogTapeInitRow, CommittedCopyState,
    FormatId, InitDecision, TapeInitCatalogProjection, TapeInitError, TapeInitGeometry,
    TapeInitWriteAction, TapeInitWriteError, TapeInitWriteOptions,
    CANONICAL_ADOPTION_BLOCK_SIZE_BYTES,
};
pub use write_session_ingress::WriteSessionApi;

#[cfg(test)]
mod tests;
