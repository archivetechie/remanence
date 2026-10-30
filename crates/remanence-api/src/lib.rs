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
pub use write_owner::terminal_inventory::{
    terminal_verification_to_proto, verification_io_to_proto,
};

mod api_state;
mod append_admission;
mod append_request;
mod append_ring;
mod append_spool;
mod audit_projection;
mod audit_query_service;
mod auth;
mod calibration;
mod catalog_conversion;
pub use append_admission::{admit_checkpointed_tape, admit_initialized_tape, AppendAdmission};
#[cfg(test)]
extern crate self as remanence_api;
mod catalogless_restore;
pub use catalogless_restore::{
    restore_catalogless, restore_catalogless_selected, scan_and_restore,
};
mod catalog_request;
mod daemon_catalog_services;
mod diagnostics;
mod direct_replay_fault;
mod drive_collection;
mod drive_mode;
mod drive_pool;
mod hex_encoding;
mod reader_hash;
pub use reader_hash::sha256_reader;
mod io_memory;
mod library;
mod live_status;
mod mount;
mod mount_backend;
pub mod mount_support;
mod standalone_init;
mod standalone_write;
pub use mount_support::{
    ChangerMount, MountBackend, MountEndpoint, MountError, MountTarget, MountedTape, OperatorMount,
};
pub use standalone_write::{MountedWriteResult, StandaloneFinalizeRequest};
mod object_fault;
mod object_position;
pub use object_position::{position_to_object, BotRelativeTapeFile, ObjectPositionOrigin};
mod operations;
mod pool_selection;
mod pool_write;
pub mod read_core;
mod read_plan;
mod read_session_service;
pub mod readiness_wait;
mod startup_checkpoint;
mod startup_guard;
mod startup_media_readiness;
mod tape_identity;
mod tape_init;
mod tape_verification;
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
    lto_generation_from_density, lto_generation_from_drive_product, raw_capacity_bytes,
    replay_committed_pool_write_from_state, seal_decision_after_write, select_tape_in_pool,
    select_tape_in_pool_for_write_session, tape_generation, verify_tape_identity,
    write_tape_bootstrap, write_to_selected_drive_checkpointed, LtoGen, MediaObservation,
    ObjectWriteMediaError, PoolWriteError, PoolWriteObjectCopyRecord, PoolWriteObjectRecord,
    PoolWriteRepresentation, PoolWriteResources, PoolWriteResult, SelectTapeError, SelectedTape,
    StreamedWriteSource, TapeIdentityError, TapePositionAfterWrite, TapeSealReason, TapeUuid,
    WritabilityError, WriteObjectInputKind, WriteObjectSource, WriteObjectToPoolRequest,
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
    maybe_write_tape_init_bootstrap, maybe_write_tape_init_bootstrap_on_drive,
    probe_bot_identity_from_source, project_tape_init_catalog_inputs, sniff,
    BootstrapAdoptionProjection, BootstrapTailClassification, BotClassification,
    BotIdentityClassification, BotInitProjection, CatalogBarcodeRelation, CatalogRowDisposition,
    CatalogTapeInitRow, CommittedCopyState, FormatId, InitDecision, LabelLifecycleState,
    TapeInitCatalogProjection, TapeInitError, TapeInitGeometry, TapeInitWriteAction,
    TapeInitWriteError, TapeInitWriteOptions, CANONICAL_ADOPTION_BLOCK_SIZE_BYTES,
};
pub use write_session_ingress::WriteSessionApi;

#[cfg(test)]
mod tests;

// Compile CLI sources here to exercise private Layer-5 entry points with the same resume vectors.
// Share the image builder and portable adapter with the fixture generator. This
// is test-only: Layer-5 resume entry points remain private to write_owner.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../remanence-cli/src/dev/resume_vectors.rs"]
mod resume_vectors;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../remanence-cli/src/dev/tape_image.rs"]
mod tape_image;
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../remanence-cli/src/dev/tape_image_vectors.rs"]
mod tape_image_vectors;

/// Discovery facts for CLI envelopes, independent of binary or daemon capabilities.
pub fn observed_endpoint_json(
    report: &remanence_library::DiscoveryReport,
) -> Vec<serde_json::Value> {
    let mut endpoints = Vec::new();
    for library in &report.libraries {
        endpoints.push(
            serde_json::json!({"kind":"library","serial":library.serial,"path":library.changer_sg}),
        );
        for bay in &library.drive_bays {
            endpoints.push(serde_json::json!({"kind":"library","library_serial":library.serial,"bay":bay.element_address,"drive_serial":bay.installed.as_ref().map(|d| &d.serial),"path":bay.installed.as_ref().and_then(|d|d.sg_path.as_ref()),"ownership":"library"}));
        }
    }
    for drive in &report.unattached_drives {
        endpoints.push(serde_json::json!({"kind":if drive.ownership == remanence_library::DriveOwnership::Standalone {"standalone"} else {"unknown"},"serial":drive.serial,"path":drive.sg_path,"ownership":if drive.ownership == remanence_library::DriveOwnership::Standalone {"standalone"} else {"unknown"}}));
    }
    endpoints
}

/// Validate a spooled canonical object against its catalog manifest before extraction.
pub fn validate_spooled_object(
    path: &std::path::Path,
    block_size: u32,
    anchor: Option<[u8; 32]>,
) -> Result<remanence_format::RemTarStreamReport, String> {
    let mut source = remanence_library::FileBlockSource::open(path, block_size as usize)
        .map_err(|e| e.to_string())?;
    let blocks = source.block_count();
    remanence_format::stream_rem_tar_object_with_manifest_anchor(
        &mut source,
        block_size as usize,
        blocks,
        &mut pool_write::ValidateCanonicalEntrySink,
        anchor,
    )
    .map_err(|e| e.to_string())
}

/// Standalone fence admission and reporting.
pub use mount_support::{standalone_fence_policy, standalone_fence_releasable};

/// In-process ownership of today's session engine.
pub mod engine;
