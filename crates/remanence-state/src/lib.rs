//! Layer 4 local daemon state for Remanence.
//!
//! This crate owns operator configuration, the exclusive local state lock, the
//! append-only audit log, and rebuildable query projections. The first slice
//! implements the state-directory and config boundary; audit and index modules
//! are added in later implementation steps.

pub mod adapters;
pub mod audit;
pub mod checkpoint;
pub mod config;
pub mod error;
pub mod index;
pub mod lock;
pub mod paths;
pub mod state;

pub use adapters::{
    append_library_audit_event, append_parity_recovery_event, append_sidecar_metadata_health_event,
    library_audit_event_record, parity_recovery_audit_record, sidecar_metadata_health_audit_record,
    Layer2AuditEventTag, Layer2AuditOpTag, Layer2AuditOutcomeTag, Layer2RescanWarningTag,
    Layer2SpaceKindTag, Layer3cAuditEventTag, Layer3cRecoveryOutcomeTag,
    Layer3cSidecarMetadataHealthTag, SharedAuditAdapter, StripePositionTag,
};
pub use audit::{
    AuditActor, AuditEvent, AuditEventRecord, AuditReceipt, AuditRecord, AuditSink, AuditSubject,
    FileAuditLog, SourceLayer,
};
pub use checkpoint::{
    list_checkpoint_journals, tape_uuid_from_checkpoint_path,
    CheckpointBootstrapObjectRepresentation, CheckpointBootstrapObjectRow, CheckpointJournalRecord,
    CheckpointObjectProjection, FileCheckpointJournal,
};
pub use config::{
    derive_tape_pool_from_voltag, load_config, parse_config_toml, validate_block_size,
    validate_config, validate_tape_pool_capacity_invariant, validate_trusted_volume_paths,
    watermark_floor_bytes, AppendStagingMode, AuditConfig, CacheConfig, CleaningConfig,
    DaemonConfig, DaemonTlsConfig, DrivesConfig, IndexConfig, JournalConfig, LibraryConfig,
    LiveStatusConfig, PoolSelectionPolicyName, RemConfig, TapeIoConfig, TapePoolConfig,
    TapePoolRuleConfig, DEFAULT_APPEND_RING_BYTES, DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
    DEFAULT_CHECKPOINT_MAX_BYTES, DEFAULT_CHECKPOINT_MAX_OBJECTS,
    DEFAULT_DRIVE_IDLE_UNLOAD_SECONDS, DEFAULT_IO_MEMORY_CEILING_BYTES,
    DEFAULT_RANGED_POSITION_CHECK_BYTES, DEFAULT_READ_RESERVOIR_BYTES,
    DEFAULT_TAPE_BLOCK_SIZE_BYTES,
};
pub use error::StateError;
pub use index::{
    AlarmRecord, AuditReplayReport, CatalogIndex, CatalogUnitFilter, CatalogUnitRecord,
    DriveAnnotationInput, DriveCorrelationRollupRecord, DriveEventRecord, DriveHealthSnapshotInput,
    DriveHealthSnapshotRecord, DriveObservationInput, DriveObservationOutcome, DriveRecord,
    ForeignArchiveProjectionInput, MediaReadinessOperationInput, MediaReadinessOperationRecord,
    MediaReadinessTransitionInput, NativeObjectCopyProjectionInput, NativeObjectCopyRecord,
    NativeObjectFileProjectionInput, NativeObjectFileRecord, NativeObjectProjectionInput,
    NativeObjectRecord, OperationRecord, ProvisionTapeInput, RebuildReport,
    RebuildTapeJournalInput, RestartOperation, RestartSession, RetireDriveOutcome, RetireTapeInput,
    RetireTapeOutcome, TapeFileRecord, TapeIoFenceInput, TapeIoFenceRecord, TapeJournalIndexInput,
    TapeJournalIndexReport, TapeKindFilter, TapePoolProjectionInput, TapePoolRecord, TapeRecord,
    DIGEST_ALGORITHM_SHA256, OBJECT_COPY_REPRESENTATION_ENCRYPTED,
    OBJECT_COPY_REPRESENTATION_PLAINTEXT, OBJECT_COPY_REPRESENTATION_UNKNOWN, SCHEMA_VERSION,
};
pub use lock::StateLockGuard;
pub use paths::StatePaths;
pub use state::{
    StartupReplayReport, StateConfigWarning, StateHandle, TapeJournalIngestionOutcome,
};
