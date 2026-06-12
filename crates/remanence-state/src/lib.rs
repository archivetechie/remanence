//! Layer 4 local daemon state for Remanence.
//!
//! This crate owns operator configuration, the exclusive local state lock, the
//! append-only audit log, and rebuildable query projections. The first slice
//! implements the state-directory and config boundary; audit and index modules
//! are added in later implementation steps.

pub mod adapters;
pub mod audit;
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
pub use config::{
    derive_tape_pool_from_voltag, load_config, parse_config_toml, validate_block_size,
    validate_config, validate_tape_pool_capacity_invariant, validate_trusted_volume_paths,
    watermark_floor_bytes, AuditConfig, CacheConfig, DaemonConfig, DaemonTlsConfig, IndexConfig,
    JournalConfig, LibraryConfig, PoolSelectionPolicyName, RemConfig, TapePoolConfig,
    TapePoolRuleConfig, DEFAULT_TAPE_BLOCK_SIZE_BYTES,
};
pub use error::StateError;
pub use index::{
    AuditReplayReport, CatalogIndex, CatalogUnitFilter, CatalogUnitRecord,
    ForeignArchiveProjectionInput, NativeObjectCopyProjectionInput, NativeObjectCopyRecord,
    NativeObjectProjectionInput, NativeObjectRecord, OperationRecord, ProvisionTapeInput,
    RebuildReport, RebuildTapeJournalInput, RestartOperation, RestartSession, RetireTapeInput,
    RetireTapeOutcome, TapeFileRecord, TapeJournalIndexInput, TapeJournalIndexReport,
    TapePoolProjectionInput, TapePoolRecord, TapeRecord, OBJECT_COPY_REPRESENTATION_ENCRYPTED,
    OBJECT_COPY_REPRESENTATION_PLAINTEXT, OBJECT_COPY_REPRESENTATION_UNKNOWN, SCHEMA_VERSION,
};
pub use lock::StateLockGuard;
pub use paths::StatePaths;
pub use state::{
    StartupReplayReport, StateConfigWarning, StateHandle, TapeJournalIngestionOutcome,
};
