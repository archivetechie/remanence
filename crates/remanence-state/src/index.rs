//! Rebuildable SQLite projection index.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use remanence_parity::{
    BootstrapObjectRepresentation, CommittedBundle, CommittedState, ParityConfig, ParityScheme,
    TapeFileEntry, TapeFileKind,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::{AuditActor, AuditEvent, AuditRecord};
use crate::config::{derive_tape_pool_from_voltag, TapePoolRuleConfig};
use crate::error::StateError;

/// Current Layer 4 SQLite schema version.
pub const SCHEMA_VERSION: u32 = 8;
const LEGACY_TAPE_POOL_MEMBERSHIPS_TABLE: &str = concat!("tape_pool_", "memberships");
/// Catalog value for an unencrypted RAO copy row.
pub const OBJECT_COPY_REPRESENTATION_PLAINTEXT: &str = "plaintext";
/// Catalog value for an encrypted RAO copy row.
pub const OBJECT_COPY_REPRESENTATION_ENCRYPTED: &str = "encrypted";
/// Catalog value for journal-discovered object copies whose RAO envelope row is unavailable.
pub const OBJECT_COPY_REPRESENTATION_UNKNOWN: &str = "unknown";

/// Typed owner for the rebuildable SQLite catalog projection.
#[derive(Debug)]
pub struct CatalogIndex {
    conn: Connection,
    path: PathBuf,
}

/// Input metadata for indexing one consumed 3c journal replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeJournalIndexInput {
    /// Tape UUID from the 3c journal header.
    pub tape_uuid: [u8; 16],
    /// Fixed block size recorded for the tape.
    pub block_size: u32,
    /// Parity scheme recorded for the tape, or `None` for no-parity tapes.
    pub scheme: Option<ParityScheme>,
    /// Journal byte offset consumed by replay.
    pub journal_offset_bytes: u64,
}

/// Report from a journal projection update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeJournalIndexReport {
    /// Whether ingestion is pending because a live writer holds the journal.
    pub ingestion_pending: bool,
    /// Number of tape-file rows written.
    pub tape_files_rebuilt: u64,
    /// Number of object-copy rows written.
    pub object_copies_rebuilt: u64,
}

/// Report from rebuilding audit-derived projections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditReplayReport {
    /// Number of audit records consumed.
    pub audit_records_replayed: u64,
    /// Number of operation rows present after replay.
    pub operations_rebuilt: u64,
    /// Number of session rows present after replay.
    pub sessions_rebuilt: u64,
    /// Number of idempotency rows present after replay.
    pub idempotency_keys_rebuilt: u64,
}

/// Operator/orchestrator pool projection for tape eligibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapePoolRecord {
    /// Stable daemon-local pool id.
    pub pool_id: String,
    /// Human-readable label.
    pub display_name: Option<String>,
    /// Optional copy segregation axis, such as `copy-a` or `offsite`.
    pub copy_class: Option<String>,
    /// Optional content segregation axis, such as `camera` or `finance`.
    pub content_class: Option<String>,
    /// Projection row creation timestamp.
    pub created_at_utc: String,
}

/// Pool definition supplied by operator config or audit replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapePoolProjectionInput {
    /// Stable daemon-local pool id.
    pub pool_id: String,
    /// Human-readable label.
    pub display_name: Option<String>,
    /// Optional copy segregation axis.
    pub copy_class: Option<String>,
    /// Optional content segregation axis.
    pub content_class: Option<String>,
    /// Row creation timestamp. Uses current UTC when omitted.
    pub created_at_utc: Option<String>,
}

/// Metadata needed to register a blank or ready tape in the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionTapeInput {
    /// Tape UUID expected in the on-tape bootstrap.
    pub tape_uuid: [u8; 16],
    /// Operator-facing barcode/volume tag.
    pub voltag: String,
    /// Fixed block size to write on the tape.
    pub block_size: u32,
    /// Parity geometry to record, or `None` for no-parity tapes.
    pub parity: ParityConfig,
    /// Permit geometry or UUID replacement even when the prior row is written.
    pub force: bool,
}

/// Request to permanently end one tape identity's life in the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetireTapeInput {
    /// Tape UUID of the identity to retire.
    pub tape_uuid: [u8; 16],
    /// Operator-supplied reason, such as `recycled` or `vtl-rebuilt`.
    pub reason: String,
}

/// Result of a retire request against the catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetireTapeOutcome {
    /// False when the tape was already retired (idempotent no-op).
    pub newly_retired: bool,
    /// Voltag detached by this retire, when one was attached.
    pub released_voltag: Option<String>,
    /// Number of committed object copies transitioned to `missing`.
    pub copies_marked_missing: u64,
}

/// One authoritative tape-journal replay to include in a full index rebuild.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildTapeJournalInput {
    /// Journal metadata needed by the SQLite projection.
    pub input: TapeJournalIndexInput,
    /// Committed state loaded from the journal by Layer 3c.
    pub state: CommittedState,
}

/// Report from rebuilding SQLite from authoritative audit and journal sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildReport {
    /// Number of tape journals consumed.
    pub tapes_rebuilt: u64,
    /// Number of tape-file rows rebuilt.
    pub tape_files_rebuilt: u64,
    /// Number of object-copy rows rebuilt.
    pub object_copies_rebuilt: u64,
    /// Number of audit records replayed.
    pub audit_records_replayed: u64,
    /// Number of tape-journal files replayed.
    pub journal_records_replayed: u64,
}

/// Non-terminal operation found during startup replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartOperation {
    /// Operation UUID to mark terminal after restart.
    pub operation_id: Uuid,
    /// Last projected operation kind.
    pub operation_kind: String,
    /// Session UUID attached to the operation, if any.
    pub session_id: Option<Uuid>,
    /// Idempotency key attached to this operation, if one is still in progress.
    pub idempotency_key: Option<Uuid>,
    /// Actor fingerprint for the idempotency key scope, if present.
    pub actor_fingerprint: Option<String>,
    /// Last projected subject string, if any.
    pub subject: Option<String>,
}

/// One projected operation row for Layer 5 status queries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationRecord {
    /// Operation UUID as canonical text.
    pub operation_id: String,
    /// Stable operation kind.
    pub operation_kind: String,
    /// Projected state string from the audit log.
    pub state: String,
    /// Session UUID attached to the operation, if any.
    pub session_id: Option<String>,
    /// Projected subject string, if any.
    pub subject: Option<String>,
    /// Operation creation/start timestamp.
    ///
    /// For operations observed from `RequestReceived`, this is the request
    /// registration timestamp. For legacy rows first observed from
    /// `OperationStarted`, it is the start timestamp.
    pub started_at_utc: String,
    /// Last projected update timestamp.
    pub updated_at_utc: String,
}

/// Non-terminal session found during startup replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartSession {
    /// Session UUID to mark lost after restart.
    pub session_id: Uuid,
    /// Last projected session kind.
    pub session_kind: String,
    /// Tape UUID attached to the session, if any.
    pub tape_uuid: Option<Vec<u8>>,
    /// Library serial attached to the session, if any.
    pub library_serial: Option<String>,
    /// Drive bay attached to the session, if any.
    pub drive_bay: Option<i64>,
}

/// Origin selector for the cross-source catalog unit query surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogUnitFilter {
    /// Return native and foreign units.
    All,
    /// Return only native Remanence object units.
    NativeObjects,
    /// Return only foreign archive units.
    ForeignArchives,
}

/// One source-neutral catalog unit row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogUnitRecord {
    /// Stable unit id in the local catalog projection.
    pub unit_id: String,
    /// Tape UUID that owns this concrete unit/copy.
    pub tape_uuid: Vec<u8>,
    /// `native_object` or `foreign_archive`.
    pub origin_kind: String,
    /// Body or archive format id.
    pub format_id: String,
    /// Native object id when `origin_kind == native_object`.
    pub native_object_id: Option<String>,
    /// Foreign scan id when `origin_kind == foreign_archive`.
    pub scan_id: Option<String>,
    /// Foreign source kind, such as `byte_stream_dump`.
    pub source_kind: Option<String>,
    /// Trusted daemon-local source id or path token for foreign refresh.
    ///
    /// This must come from daemon configuration or another privileged local
    /// source. It is not safe to accept arbitrary client-submitted paths here.
    pub source_id: Option<String>,
    /// Foreign scan confidence.
    pub confidence: Option<String>,
    /// Last known entry count for a foreign scan.
    pub entry_count: Option<u64>,
    /// Last known damage event count for a foreign scan.
    pub damage_event_count: Option<u64>,
    /// Last scan timestamp for a foreign unit.
    pub last_scan_at_utc: Option<String>,
    /// Driver-private persisted state for foreign units.
    pub adapter_state: Vec<u8>,
    /// Projection row creation timestamp.
    pub created_at_utc: String,
}

/// Native object row ready for the Layer 5 object catalog API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeObjectRecord {
    /// Remanence object UUID.
    pub object_id: String,
    /// Opaque caller/orchestrator object id.
    pub caller_object_id: Option<String>,
    /// Native body format id.
    pub body_format: String,
    /// Logical payload size if known.
    pub logical_size_bytes: Option<u64>,
    /// Content hash if known.
    pub content_hash: Option<Vec<u8>>,
    /// Metadata hash if known.
    pub metadata_hash: Option<Vec<u8>>,
    /// Creation timestamp.
    pub created_at_utc: String,
    /// Known object copies.
    pub copies: Vec<NativeObjectCopyRecord>,
}

/// Native object-copy locator and status row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeObjectCopyRecord {
    /// Remanence object UUID.
    pub object_id: String,
    /// Tape UUID containing this copy.
    pub tape_uuid: Vec<u8>,
    /// Filemark-delimited tape-file number.
    pub tape_file_number: u32,
    /// First object-local body LBA containing payload data.
    pub first_body_lba: u64,
    /// First parity data ordinal, if known.
    pub first_parity_data_ordinal: Option<u64>,
    /// Protection watermark when the copy row was projected.
    pub protected_until_ordinal: Option<u64>,
    /// Projected copy status.
    pub status: String,
    /// Pool of the tape when this copy was committed, if assigned.
    pub pool_id: Option<String>,
    /// RAO representation stored in this copy: `plaintext`, `encrypted`, or `unknown`.
    pub representation: String,
    /// Opaque 16-byte RAO key id for encrypted copies.
    pub key_id: Option<Vec<u8>>,
    /// Encrypted RAO metadata frame length.
    pub metadata_frame_len: Option<u64>,
    /// SHA-256 of the canonical plaintext RAO object bytes.
    pub plaintext_digest: Option<Vec<u8>>,
    /// SHA-256 of the stored representation bytes for this copy.
    pub stored_digest: Option<Vec<u8>>,
}

/// Native object member-file row for catalog-backed partial-file restore.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeObjectFileRecord {
    /// Remanence object UUID.
    pub object_id: String,
    /// Stable file identifier inside the object.
    pub file_id: String,
    /// UTF-8 path inside the object.
    pub path: String,
    /// Exact file payload size.
    pub size_bytes: u64,
    /// SHA-256 of the exact file payload bytes.
    pub file_sha256: Vec<u8>,
    /// First object-local body LBA containing file data.
    pub first_chunk_lba: Option<u64>,
    /// Number of body chunks containing file data.
    pub chunk_count: u64,
    /// Optional mtime pax value.
    pub mtime: Option<String>,
    /// Optional executable flag.
    pub executable: Option<bool>,
}

/// Object row supplied by Layer 5 after a native object commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeObjectProjectionInput {
    /// Remanence object UUID.
    pub object_id: String,
    /// Opaque caller/orchestrator object id.
    pub caller_object_id: Option<String>,
    /// Native body format id.
    pub body_format: String,
    /// Logical payload size if known.
    pub logical_size_bytes: Option<u64>,
    /// Content hash if known.
    pub content_hash: Option<Vec<u8>>,
    /// Metadata hash if known.
    pub metadata_hash: Option<Vec<u8>>,
    /// Creation timestamp. Uses current UTC when omitted.
    pub created_at_utc: Option<String>,
}

/// Object-copy row supplied by Layer 5 after a native object commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeObjectCopyProjectionInput {
    /// Remanence object UUID.
    pub object_id: String,
    /// Tape UUID containing this copy.
    pub tape_uuid: [u8; 16],
    /// Filemark-delimited tape-file number.
    pub tape_file_number: u32,
    /// First object-local body LBA containing payload data.
    pub first_body_lba: u64,
    /// First parity data ordinal, if known.
    pub first_parity_data_ordinal: Option<u64>,
    /// Protection watermark when the copy row was projected.
    pub protected_until_ordinal: Option<u64>,
    /// Projected copy status.
    pub status: String,
    /// RAO representation stored in this copy: `plaintext` or `encrypted`.
    pub representation: String,
    /// Opaque 16-byte RAO key id for encrypted copies.
    pub key_id: Option<Vec<u8>>,
    /// Encrypted RAO metadata frame length.
    pub metadata_frame_len: Option<u64>,
    /// SHA-256 of the canonical plaintext RAO object bytes.
    pub plaintext_digest: Option<Vec<u8>>,
    /// SHA-256 of the stored representation bytes for this copy.
    pub stored_digest: Option<Vec<u8>>,
}

/// Member-file row supplied by Layer 5 after a native object commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeObjectFileProjectionInput {
    /// Remanence object UUID.
    pub object_id: String,
    /// Stable file identifier inside the object.
    pub file_id: String,
    /// UTF-8 path inside the object.
    pub path: String,
    /// Exact file payload size.
    pub size_bytes: u64,
    /// SHA-256 of the exact file payload bytes.
    pub file_sha256: Vec<u8>,
    /// First object-local body LBA containing file data.
    pub first_chunk_lba: Option<u64>,
    /// Number of body chunks containing file data.
    pub chunk_count: u64,
    /// Optional mtime pax value.
    pub mtime: Option<String>,
    /// Optional executable flag.
    pub executable: Option<bool>,
}

/// Foreign archive scan summary supplied by a registered read-only driver.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignArchiveProjectionInput {
    /// Optional tape UUID when the source is a physical cartridge.
    pub tape_uuid: Vec<u8>,
    /// Format identifier, such as `remanence-bru`.
    pub format_id: String,
    /// Stable scan id assigned by the caller/daemon.
    pub scan_id: String,
    /// Source kind, such as `byte_stream_dump` or `physical_tape_records`.
    pub source_kind: String,
    /// Trusted daemon-local source id or path token.
    ///
    /// This must come from daemon configuration or another privileged local
    /// source. It is not safe to accept arbitrary client-submitted paths here.
    pub source_id: String,
    /// Scan confidence: `low`, `medium`, or `high`.
    pub confidence: String,
    /// Number of normalized entries seen during scan.
    pub entry_count: u64,
    /// Number of non-fatal damage events seen during scan.
    pub damage_event_count: u64,
    /// Driver-private state needed to resume or refresh this scan.
    pub adapter_state: Vec<u8>,
    /// Scan timestamp. Uses current UTC when omitted.
    pub last_scan_at_utc: Option<String>,
    /// Row creation timestamp. Uses current UTC when omitted.
    pub created_at_utc: Option<String>,
}

/// One tape row from the rebuildable catalog projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeRecord {
    /// Remanence tape UUID.
    pub tape_uuid: Vec<u8>,
    /// Operator-facing voltag when known.
    pub voltag: Option<String>,
    /// Current tape-pool assignment, if any.
    pub pool_id: Option<String>,
    /// Dominant native body format derived from cataloged objects on this tape.
    pub body_format: Option<String>,
    /// Fixed block size when known.
    pub block_size: Option<u64>,
    /// Parity scheme identifier when known.
    pub scheme_id: Option<String>,
    /// Data blocks per stripe when known.
    pub data_blocks_per_stripe: Option<u32>,
    /// Parity blocks per stripe when known.
    pub parity_blocks_per_stripe: Option<u32>,
    /// Stripes per neighborhood when known.
    pub stripes_per_neighborhood: Option<u32>,
    /// Last committed filemark-delimited tape-file number.
    pub last_committed_tape_file: Option<u64>,
    /// Total committed object-data ordinals on this tape.
    pub total_committed_ordinals: u64,
    /// Projection state.
    pub state: String,
    /// Last projection update timestamp.
    pub updated_at_utc: String,
}

/// One tape-file row from the rebuildable catalog projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeFileRecord {
    /// Remanence tape UUID.
    pub tape_uuid: Vec<u8>,
    /// Filemark-delimited tape-file number.
    pub tape_file_number: u32,
    /// Tape-file kind string.
    pub kind: String,
    /// Number of fixed-size blocks in this tape file.
    pub block_count: u64,
    /// Object id when kind is `object`.
    pub object_id: Option<String>,
}

impl CatalogIndex {
    /// Open the SQLite projection and apply idempotent migrations.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|err| StateError::io_at("create sqlite directory", parent, err))?;
        }
        let conn = Connection::open(&path).map_err(|err| sqlite_open_error(&path, err))?;
        configure_sqlite(&conn)?;
        migrate(&conn)?;
        Ok(Self { conn, path })
    }

    /// Open the SQLite projection for read-only query serving.
    ///
    /// This does not create directories or run migrations. The owner process
    /// must open the index read-write first so schema creation and migration
    /// happen at one explicit point.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, StateError> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|err| sqlite_open_error(&path, err))?;
        configure_read_only_sqlite(&conn)?;
        validate_schema(&conn)?;
        Ok(Self { conn, path })
    }

    /// Path backing this projection.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// SQLite `PRAGMA user_version` after migration.
    pub fn schema_version(&self) -> Result<u32, StateError> {
        self.conn
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .map_err(|err| sqlite_error("read sqlite user_version", err))
    }

    /// Return whether a projection table exists.
    pub fn table_exists(&self, table_name: &str) -> Result<bool, StateError> {
        self.conn
            .query_row(
                "select 1 from sqlite_master where type = 'table' and name = ?1",
                params![table_name],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(|err| sqlite_error("check sqlite table existence", err))
    }

    /// Run SQLite quick_check and return the result text.
    pub fn quick_check(&self) -> Result<String, StateError> {
        self.conn
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
            .map_err(|err| sqlite_error("sqlite quick_check", err))
    }

    /// Index one fully replayed 3c committed state.
    pub fn index_committed_tape_journal(
        &mut self,
        input: TapeJournalIndexInput,
        state: &CommittedState,
    ) -> Result<TapeJournalIndexReport, StateError> {
        let updated_at = now_utc()?;
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin journal ingestion transaction", err))?;
        let report = index_committed_tape_journal_tx(&tx, &input, state, &updated_at)?;
        tx.commit()
            .map_err(|err| sqlite_error("commit journal ingestion transaction", err))?;
        Ok(report)
    }

    /// Incrementally project one atomically committed 3c tape-file bundle.
    ///
    /// Live Layer 5 writes already know the object row and the 3c commit bundle
    /// at the commit boundary. This method updates the same rebuildable tables
    /// used by full tape-journal replay, without clearing earlier rows for the
    /// tape.
    pub fn project_committed_tape_file_bundle(
        &mut self,
        input: TapeJournalIndexInput,
        bundle: &CommittedBundle,
    ) -> Result<TapeJournalIndexReport, StateError> {
        let updated_at = now_utc()?;
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin tape-file bundle projection", err))?;
        let report = project_committed_tape_file_bundle_tx(&tx, &input, bundle, &updated_at)?;
        tx.commit()
            .map_err(|err| sqlite_error("commit tape-file bundle projection", err))?;
        Ok(report)
    }

    /// Replace the structural tape-file projection for one tape.
    ///
    /// Physical reconciliation can recover filemark geometry without recovering
    /// native object identities. This method intentionally touches only the
    /// `tape_files` projection and tape-level structural watermarks; object and
    /// object-copy projections remain owned by native object commit/journal
    /// replay paths.
    pub fn reconcile_tape_files_projection(
        &mut self,
        tape_uuid: [u8; 16],
        entries: &[TapeFileEntry],
        highest_protected_ordinal: u64,
        total_committed_ordinals: u64,
    ) -> Result<TapeJournalIndexReport, StateError> {
        let updated_at = now_utc()?;
        let last_committed_tape_file = entries
            .iter()
            .map(|entry| entry.tape_file_number)
            .max()
            .map(i64::from);
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin tape-files reconciliation", err))?;
        let changed = tx
            .execute(
                "update tapes
                 set highest_protected_ordinal = ?2,
                     total_committed_ordinals = ?3,
                     last_committed_tape_file = ?4,
                     state = 'ingested',
                     updated_at_utc = ?5
                 where tape_uuid = ?1",
                params![
                    tape_uuid.to_vec(),
                    u64_to_i64(highest_protected_ordinal, "highest_protected_ordinal")?,
                    u64_to_i64(total_committed_ordinals, "total_committed_ordinals")?,
                    last_committed_tape_file,
                    updated_at,
                ],
            )
            .map_err(|err| sqlite_error("update tape during tape-files reconciliation", err))?;
        if changed == 0 {
            return Err(StateError::IndexCorrupt(format!(
                "cannot reconcile tape_files for unknown tape {}",
                hex_uuid(tape_uuid)
            )));
        }
        tx.execute(
            "delete from tape_files where tape_uuid = ?1",
            params![tape_uuid.to_vec()],
        )
        .map_err(|err| sqlite_error("clear tape_files during reconciliation", err))?;
        for entry in entries {
            insert_tape_file(&tx, tape_uuid, entry)?;
        }
        tx.commit()
            .map_err(|err| sqlite_error("commit tape-files reconciliation", err))?;
        Ok(TapeJournalIndexReport {
            ingestion_pending: false,
            tape_files_rebuilt: entries.len() as u64,
            object_copies_rebuilt: 0,
        })
    }

    /// Register or refresh a blank/ready tape row without projecting objects.
    ///
    /// Provisioning owns only the `tapes` row. Pool membership is derived from
    /// the barcode via `[[tape_pool_rules]]` and projected onto `tapes.pool_id`
    /// by `reconcile_tape_pool_projection_from_rules`.
    pub fn provision_tape(&mut self, input: ProvisionTapeInput) -> Result<(), StateError> {
        let geometry = ProvisionTapeGeometry::from_parity(input.block_size, &input.parity)?;
        let voltag = input.voltag.trim().to_string();
        if voltag.is_empty() {
            return Err(StateError::ConfigInvalid(
                "provision_tape requires a non-empty voltag".to_string(),
            ));
        }
        let updated_at = now_utc()?;
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin tape provisioning transaction", err))?;
        provision_tape_tx(
            &tx,
            input.tape_uuid,
            voltag.as_str(),
            &geometry,
            input.force,
            updated_at.as_str(),
        )?;
        tx.commit()
            .map_err(|err| sqlite_error("commit tape provisioning transaction", err))?;
        Ok(())
    }

    /// Mark a tape closed to future writes while preserving its catalog rows.
    ///
    /// The pool selector treats any non-`ready` tape as unwritable, so this
    /// state transition is enough to exclude a tape after eager sealing.
    pub fn seal_tape(&mut self, tape_uuid: [u8; 16]) -> Result<(), StateError> {
        let updated_at = now_utc()?;
        let changed = self
            .conn
            .execute(
                "update tapes
                 set state = 'sealed',
                     updated_at_utc = ?2
                 where tape_uuid = ?1",
                params![tape_uuid.to_vec(), updated_at],
            )
            .map_err(|err| sqlite_error("seal tape", err))?;
        if changed == 0 {
            return Err(StateError::IndexCorrupt(format!(
                "cannot seal unknown tape {}",
                hex_uuid(tape_uuid)
            )));
        }
        Ok(())
    }

    /// Permanently retire one tape identity and mark its copies missing.
    ///
    /// `retired` is terminal: no code path leaves it, and `provision_tape`
    /// refuses to reuse the row even with `force`. The voltag is detached in
    /// the same transaction (the partial unique index `tapes_voltag_unique`
    /// makes detach-before-rebind mandatory); the released voltag is recorded
    /// in the returned outcome for the audit detail, not in a column.
    /// `pool_id` is kept as history — selection gates on state, so a retired
    /// row in a pool is harmless and the provenance is useful.
    ///
    /// Invariant: copy status is derived from tape state. A copy on a retired
    /// tape is `missing`, always; this method enforces it for the live
    /// catalog and rebuild re-derives it after journal replay, so no
    /// copy-level preservation machinery is needed.
    pub fn retire_tape(&mut self, input: RetireTapeInput) -> Result<RetireTapeOutcome, StateError> {
        let updated_at = now_utc()?;
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin tape retire transaction", err))?;
        let existing: Option<(Option<String>, String)> = tx
            .query_row(
                "select voltag, state from tapes where tape_uuid = ?1",
                params![input.tape_uuid.to_vec()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|err| sqlite_error("query tape for retire", err))?;
        let Some((voltag, state)) = existing else {
            return Err(StateError::IndexCorrupt(format!(
                "cannot retire unknown tape {}",
                hex_uuid(input.tape_uuid)
            )));
        };
        // Idempotent: recycle scripts re-run safely against an already
        // retired identity without producing a second state transition.
        if state == "retired" {
            return Ok(RetireTapeOutcome {
                newly_retired: false,
                released_voltag: None,
                copies_marked_missing: 0,
            });
        }
        tx.execute(
            "update tapes
             set state = 'retired',
                 voltag = null,
                 updated_at_utc = ?2
             where tape_uuid = ?1",
            params![input.tape_uuid.to_vec(), updated_at],
        )
        .map_err(|err| sqlite_error("retire tape", err))?;
        let copies_marked_missing = tx
            .execute(
                "update object_copies
                 set status = 'missing'
                 where tape_uuid = ?1 and status = 'committed'",
                params![input.tape_uuid.to_vec()],
            )
            .map_err(|err| sqlite_error("mark retired tape copies missing", err))?;
        tx.commit()
            .map_err(|err| sqlite_error("commit tape retire transaction", err))?;
        Ok(RetireTapeOutcome {
            newly_retired: true,
            released_voltag: voltag,
            copies_marked_missing: copies_marked_missing as u64,
        })
    }

    /// Object ids that have copy rows but no `committed` copy anywhere.
    ///
    /// This is the degraded-objects hook for retire reporting and future
    /// self-heal: objects listed here exist in the catalog but cannot be
    /// read back from any tape.
    pub fn list_objects_with_no_committed_copies(&self) -> Result<Vec<String>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select distinct object_id
                 from object_copies
                 where object_id not in (
                   select object_id from object_copies where status = 'committed'
                 )
                 order by object_id",
            )
            .map_err(|err| sqlite_error("prepare degraded object query", err))?;
        let mut rows = stmt
            .query([])
            .map_err(|err| sqlite_error("query degraded objects", err))?;
        let mut object_ids = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate degraded objects", err))?
        {
            object_ids.push(row_get(row, 0, "object_copies.object_id")?);
        }
        Ok(object_ids)
    }

    /// Project a native object commit and its 3c tape-file bundle atomically.
    ///
    /// Live Layer 5 writes produce both projections at one commit boundary.
    /// This method keeps the object/copy rows and tape-file bundle rows in one
    /// SQLite transaction so a later bundle projection error cannot leave an
    /// orphan committed object copy visible in the catalog.
    pub fn project_native_object_and_committed_tape_file_bundle(
        &mut self,
        object: NativeObjectProjectionInput,
        files: &[NativeObjectFileProjectionInput],
        copies: &[NativeObjectCopyProjectionInput],
        tape_input: TapeJournalIndexInput,
        bundle: &CommittedBundle,
    ) -> Result<TapeJournalIndexReport, StateError> {
        let created_at_utc = match object.created_at_utc.as_deref() {
            Some(value) => value.to_string(),
            None => now_utc()?,
        };
        let updated_at = now_utc()?;
        let tx = self.conn.transaction().map_err(|err| {
            sqlite_error("begin native object and tape-file bundle projection", err)
        })?;
        upsert_native_object_projection_tx(
            &tx,
            &object,
            Some(files),
            copies,
            created_at_utc.as_str(),
        )?;
        let report =
            project_committed_tape_file_bundle_tx(&tx, &tape_input, bundle, updated_at.as_str())?;
        tx.commit().map_err(|err| {
            sqlite_error("commit native object and tape-file bundle projection", err)
        })?;
        Ok(report)
    }

    /// Mark a tape journal as pending because a live append session owns it.
    pub fn mark_tape_journal_ingestion_pending(
        &mut self,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: &ParityScheme,
    ) -> Result<TapeJournalIndexReport, StateError> {
        let updated_at = now_utc()?;
        self.conn
            .execute(
                "insert into tapes(
                   tape_uuid, block_size, scheme_id, data_blocks_per_stripe,
                   parity_blocks_per_stripe, stripes_per_neighborhood,
                   state, updated_at_utc
                 )
                 values(?1, ?2, ?3, ?4, ?5, ?6, 'ingestion_pending', ?7)
                 on conflict(tape_uuid) do update set
                   state = excluded.state,
                   updated_at_utc = excluded.updated_at_utc",
                params![
                    tape_uuid.to_vec(),
                    i64::from(block_size),
                    scheme.id.as_str(),
                    i64::from(scheme.data_blocks_per_stripe),
                    i64::from(scheme.parity_blocks_per_stripe),
                    i64::from(scheme.stripes_per_neighborhood),
                    updated_at,
                ],
            )
            .map_err(|err| sqlite_error("mark tape journal ingestion pending", err))?;
        Ok(TapeJournalIndexReport {
            ingestion_pending: true,
            tape_files_rebuilt: 0,
            object_copies_rebuilt: 0,
        })
    }

    /// Rebuild audit-derived operation, session, and idempotency projections.
    pub fn replay_audit_records(
        &mut self,
        records: &[AuditRecord],
    ) -> Result<AuditReplayReport, StateError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin audit replay transaction", err))?;
        tx.execute("delete from idempotency_keys", [])
            .map_err(|err| sqlite_error("clear idempotency projection", err))?;
        tx.execute("delete from operations", [])
            .map_err(|err| sqlite_error("clear operations projection", err))?;
        tx.execute("delete from sessions", [])
            .map_err(|err| sqlite_error("clear sessions projection", err))?;

        for record in records {
            project_session_record(&tx, record)?;
            project_operation_record(&tx, record)?;
            project_idempotency_record(&tx, record, IdempotencyProjectionMode::Replay)?;
        }

        let operations_rebuilt = table_count(&tx, "operations")?;
        let sessions_rebuilt = table_count(&tx, "sessions")?;
        let idempotency_keys_rebuilt = table_count(&tx, "idempotency_keys")?;
        tx.commit()
            .map_err(|err| sqlite_error("commit audit replay transaction", err))?;

        Ok(AuditReplayReport {
            audit_records_replayed: records.len() as u64,
            operations_rebuilt,
            sessions_rebuilt,
            idempotency_keys_rebuilt,
        })
    }

    /// Rebuild all Layer 4-owned projections from authoritative sources.
    pub fn rebuild_from_authoritative_sources(
        &mut self,
        audit_records: &[AuditRecord],
        tape_journals: &[RebuildTapeJournalInput],
    ) -> Result<RebuildReport, StateError> {
        let updated_at = now_utc()?;
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin full index rebuild transaction", err))?;
        let preserved_tapes = query_preserved_tape_rows_tx(&tx)?;
        clear_rebuildable_tables(&tx)?;
        restore_preserved_tape_rows_tx(&tx, &preserved_tapes)?;

        for record in audit_records {
            project_session_record(&tx, record)?;
            project_operation_record(&tx, record)?;
            project_idempotency_record(&tx, record, IdempotencyProjectionMode::Replay)?;
        }

        let mut tape_files_rebuilt = 0u64;
        let mut object_copies_rebuilt = 0u64;
        for journal in tape_journals {
            let report =
                index_committed_tape_journal_tx(&tx, &journal.input, &journal.state, &updated_at)?;
            tape_files_rebuilt = tape_files_rebuilt
                .checked_add(report.tape_files_rebuilt)
                .ok_or_else(|| {
                    StateError::IndexMigrationFailed("tape_files_rebuilt overflow".to_string())
                })?;
            object_copies_rebuilt = object_copies_rebuilt
                .checked_add(report.object_copies_rebuilt)
                .ok_or_else(|| {
                    StateError::IndexMigrationFailed("object_copies_rebuilt overflow".to_string())
                })?;
        }
        merge_preserved_tape_operator_columns_tx(&tx, &preserved_tapes)?;

        // Copy status is derived from tape state (a copy on a retired tape
        // is `missing`, always), so it is re-derived here after journal
        // replay re-created the copies as `committed`. This keeps copy rows
        // out of the preservation snapshot entirely.
        tx.execute(
            "update object_copies
             set status = 'missing'
             where tape_uuid in (select tape_uuid from tapes where state = 'retired')
               and status = 'committed'",
            [],
        )
        .map_err(|err| sqlite_error("re-derive retired tape copy statuses", err))?;

        tx.commit()
            .map_err(|err| sqlite_error("commit full index rebuild transaction", err))?;
        Ok(RebuildReport {
            tapes_rebuilt: tape_journals.len() as u64,
            tape_files_rebuilt,
            object_copies_rebuilt,
            audit_records_replayed: audit_records.len() as u64,
            journal_records_replayed: tape_journals.len() as u64,
        })
    }

    /// Incrementally project one newly appended audit record.
    pub fn project_audit_record(&mut self, record: &AuditRecord) -> Result<(), StateError> {
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin incremental audit projection", err))?;
        project_session_record(&tx, record)?;
        project_operation_record(&tx, record)?;
        project_idempotency_record(&tx, record, IdempotencyProjectionMode::Live)?;
        tx.commit()
            .map_err(|err| sqlite_error("commit incremental audit projection", err))?;
        Ok(())
    }

    /// Project one operator-defined tape pool from config or audit authority.
    pub fn upsert_tape_pool_projection(
        &mut self,
        input: TapePoolProjectionInput,
    ) -> Result<(), StateError> {
        let pool_id = normalize_pool_id(input.pool_id.as_str())?;
        let created_at_utc = input.created_at_utc.unwrap_or(now_utc()?);
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin tape pool projection", err))?;
        upsert_tape_pool_projection_tx(
            &tx,
            pool_id.as_str(),
            input.display_name.as_deref(),
            input.copy_class.as_deref(),
            input.content_class.as_deref(),
            created_at_utc.as_str(),
        )?;
        tx.commit()
            .map_err(|err| sqlite_error("commit tape pool projection", err))?;
        Ok(())
    }

    /// Reconcile config-authoritative tape pools and barcode-derived memberships.
    ///
    /// Pool definitions come from `[[tape_pools]]`; memberships are recomputed
    /// from current catalog tape voltags and `[[tape_pool_rules]]`. Tapes whose
    /// voltags match no rule are projected with no current pool.
    pub fn reconcile_tape_pool_projection_from_rules(
        &mut self,
        pools: &[TapePoolProjectionInput],
        rules: &[TapePoolRuleConfig],
    ) -> Result<(), StateError> {
        let normalized_pools = pools
            .iter()
            .map(|pool| {
                Ok((
                    normalize_pool_id(pool.pool_id.as_str())?,
                    pool.display_name.clone(),
                    pool.copy_class.clone(),
                    pool.content_class.clone(),
                    pool.created_at_utc.clone().unwrap_or(now_utc()?),
                ))
            })
            .collect::<Result<Vec<_>, StateError>>()?;
        let configured_pool_ids = normalized_pools
            .iter()
            .map(|(pool_id, _, _, _, _)| pool_id.clone())
            .collect::<HashSet<_>>();

        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin tape pool rule reconciliation", err))?;

        let normalized_memberships = query_tapes_for_pool_derivation_tx(&tx)?
            .into_iter()
            .filter_map(|(tape_uuid, voltag)| {
                let pool_id = derive_tape_pool_from_voltag(voltag.as_str(), rules)?;
                Some((tape_uuid, pool_id.to_string()))
            })
            .map(|(tape_uuid, pool_id)| Ok((tape_uuid, normalize_pool_id(pool_id.as_str())?)))
            .collect::<Result<Vec<_>, StateError>>()?;
        for (_, pool_id) in &normalized_memberships {
            if !configured_pool_ids.contains(pool_id) {
                return Err(StateError::ConfigInvalid(format!(
                    "derived tape pool membership references unknown pool id {pool_id}"
                )));
            }
        }
        let configured_memberships = normalized_memberships
            .iter()
            .map(|(tape_uuid, pool_id)| (tape_uuid.to_vec(), pool_id.clone()))
            .collect::<HashSet<_>>();

        let existing_memberships = query_memberships_tx(&tx)?;
        for (tape_uuid, pool_id) in existing_memberships {
            if !configured_memberships.contains(&(tape_uuid.clone(), pool_id)) {
                tx.execute(
                    "update tapes set pool_id = null where tape_uuid = ?1",
                    params![tape_uuid],
                )
                .map_err(|err| sqlite_error("clear stale derived tape pool membership", err))?;
            }
        }

        let existing_pool_ids = query_tape_pool_ids_tx(&tx)?;
        for pool_id in existing_pool_ids {
            if !configured_pool_ids.contains(&pool_id) {
                tx.execute(
                    "delete from tape_pools where pool_id = ?1",
                    params![pool_id],
                )
                .map_err(|err| sqlite_error("delete stale tape pool", err))?;
            }
        }

        for (pool_id, display_name, copy_class, content_class, created_at_utc) in normalized_pools {
            upsert_tape_pool_projection_tx(
                &tx,
                pool_id.as_str(),
                display_name.as_deref(),
                copy_class.as_deref(),
                content_class.as_deref(),
                created_at_utc.as_str(),
            )?;
        }
        for (tape_uuid, pool_id) in normalized_memberships {
            project_tape_pool_membership_tx(&tx, tape_uuid, pool_id.as_str())?;
        }

        tx.commit()
            .map_err(|err| sqlite_error("commit tape pool rule reconciliation", err))?;
        Ok(())
    }

    /// Project a tape-to-pool membership for future write eligibility.
    ///
    /// Existing object-copy rows snapshot the pool used at commit time. To
    /// avoid silently mixing data classes on one cartridge, a tape that already
    /// has committed copies in a different or unknown pool cannot be
    /// reassigned here.
    pub fn project_tape_pool_membership(
        &mut self,
        tape_uuid: [u8; 16],
        pool_id: &str,
    ) -> Result<(), StateError> {
        let pool_id = normalize_pool_id(pool_id)?;
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin tape pool membership projection", err))?;
        project_tape_pool_membership_tx(&tx, tape_uuid, pool_id.as_str())?;
        tx.commit()
            .map_err(|err| sqlite_error("commit tape pool membership projection", err))?;
        Ok(())
    }

    /// List configured tape pools.
    pub fn list_tape_pools(&self) -> Result<Vec<TapePoolRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select pool_id, display_name, copy_class, content_class, created_at_utc
                 from tape_pools
                 order by pool_id",
            )
            .map_err(|err| sqlite_error("prepare tape-pool query", err))?;
        let mut rows = stmt
            .query([])
            .map_err(|err| sqlite_error("query tape pools", err))?;
        let mut pools = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate tape pools", err))?
        {
            pools.push(tape_pool_from_row(row)?);
        }
        Ok(pools)
    }

    /// Fetch one configured tape pool.
    pub fn get_tape_pool(&self, pool_id: &str) -> Result<Option<TapePoolRecord>, StateError> {
        let pool_id = normalize_pool_id(pool_id)?;
        let mut stmt = self
            .conn
            .prepare(
                "select pool_id, display_name, copy_class, content_class, created_at_utc
                 from tape_pools
                 where pool_id = ?1",
            )
            .map_err(|err| sqlite_error("prepare tape-pool lookup", err))?;
        let mut rows = stmt
            .query(params![pool_id])
            .map_err(|err| sqlite_error("query tape-pool lookup", err))?;
        match rows
            .next()
            .map_err(|err| sqlite_error("iterate tape-pool lookup", err))?
        {
            Some(row) => Ok(Some(tape_pool_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Return the current pool assignment for one tape, if assigned.
    pub fn get_tape_pool_membership(
        &self,
        tape_uuid: &[u8; 16],
    ) -> Result<Option<String>, StateError> {
        self.conn
            .query_row(
                "select pool_id from tapes where tape_uuid = ?1 and pool_id is not null",
                params![tape_uuid.to_vec()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| sqlite_error("lookup tape pool membership", err))
    }

    /// Project a native object commit and its concrete tape copies into Layer 4.
    pub fn upsert_native_object_projection(
        &mut self,
        object: NativeObjectProjectionInput,
        copies: &[NativeObjectCopyProjectionInput],
    ) -> Result<(), StateError> {
        let created_at_utc = match object.created_at_utc.as_deref() {
            Some(value) => value.to_string(),
            None => now_utc()?,
        };
        let tx = self
            .conn
            .transaction()
            .map_err(|err| sqlite_error("begin native object projection", err))?;
        upsert_native_object_projection_tx(&tx, &object, None, copies, created_at_utc.as_str())?;
        tx.commit()
            .map_err(|err| sqlite_error("commit native object projection", err))?;
        Ok(())
    }

    /// Project a foreign archive scan unit without exposing driver-private locators.
    pub fn upsert_foreign_archive_projection(
        &mut self,
        input: ForeignArchiveProjectionInput,
    ) -> Result<String, StateError> {
        let unit_id = foreign_catalog_unit_id(
            input.source_kind.as_str(),
            input.source_id.as_str(),
            input.scan_id.as_str(),
        );
        let entry_count = u64_to_i64(input.entry_count, "entry_count")?;
        let damage_event_count = u64_to_i64(input.damage_event_count, "damage_event_count")?;
        let now = now_utc()?;
        let last_scan_at_utc = input.last_scan_at_utc.unwrap_or_else(|| now.clone());
        let created_at_utc = input.created_at_utc.unwrap_or(now);

        self.conn
            .execute(
                "insert into catalog_units(
                   unit_id, tape_uuid, origin_kind, format_id, native_object_id,
                   scan_id, source_kind, source_id, confidence, entry_count,
                   damage_event_count, last_scan_at_utc, adapter_state, created_at_utc
                 )
                 values(?1, ?2, 'foreign_archive', ?3, null, ?4, ?5, ?6, ?7,
                        ?8, ?9, ?10, ?11, ?12)
                 on conflict(unit_id) do update set
                   tape_uuid = excluded.tape_uuid,
                   format_id = excluded.format_id,
                   scan_id = excluded.scan_id,
                   source_kind = excluded.source_kind,
                   source_id = excluded.source_id,
                   confidence = excluded.confidence,
                   entry_count = excluded.entry_count,
                   damage_event_count = excluded.damage_event_count,
                   last_scan_at_utc = excluded.last_scan_at_utc,
                   adapter_state = excluded.adapter_state",
                params![
                    unit_id.as_str(),
                    input.tape_uuid,
                    input.format_id.as_str(),
                    input.scan_id.as_str(),
                    input.source_kind.as_str(),
                    input.source_id.as_str(),
                    input.confidence.as_str(),
                    entry_count,
                    damage_event_count,
                    last_scan_at_utc.as_str(),
                    input.adapter_state,
                    created_at_utc.as_str(),
                ],
            )
            .map_err(|err| sqlite_error("upsert foreign archive projection", err))?;
        Ok(unit_id)
    }

    /// List source-neutral catalog units for operator/discovery surfaces.
    pub fn list_catalog_units(
        &self,
        filter: CatalogUnitFilter,
    ) -> Result<Vec<CatalogUnitRecord>, StateError> {
        let origin_kind = match filter {
            CatalogUnitFilter::All => None,
            CatalogUnitFilter::NativeObjects => Some("native_object"),
            CatalogUnitFilter::ForeignArchives => Some("foreign_archive"),
        };
        let where_clause = if origin_kind.is_some() {
            " where origin_kind = ?1"
        } else {
            ""
        };
        let sql = format!(
            "select unit_id, tape_uuid, origin_kind, format_id, native_object_id,
                    scan_id, source_kind, source_id, confidence, entry_count,
                    damage_event_count, last_scan_at_utc, adapter_state,
                    created_at_utc
             from catalog_units{where_clause}
             order by origin_kind, created_at_utc, unit_id"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|err| sqlite_error("prepare catalog unit query", err))?;
        let mut rows = if let Some(origin_kind) = origin_kind {
            stmt.query(params![origin_kind])
        } else {
            stmt.query([])
        }
        .map_err(|err| sqlite_error("query catalog units", err))?;
        let mut units = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate catalog units", err))?
        {
            units.push(catalog_unit_from_row(row)?);
        }
        Ok(units)
    }

    /// Visit source-neutral catalog units without materializing the full query.
    pub fn for_each_catalog_unit<F>(
        &self,
        filter: CatalogUnitFilter,
        mut visit: F,
    ) -> Result<(), StateError>
    where
        F: FnMut(CatalogUnitRecord) -> ControlFlow<()>,
    {
        let origin_kind = match filter {
            CatalogUnitFilter::All => None,
            CatalogUnitFilter::NativeObjects => Some("native_object"),
            CatalogUnitFilter::ForeignArchives => Some("foreign_archive"),
        };
        let where_clause = if origin_kind.is_some() {
            " where origin_kind = ?1"
        } else {
            ""
        };
        let sql = format!(
            "select unit_id, tape_uuid, origin_kind, format_id, native_object_id,
                    scan_id, source_kind, source_id, confidence, entry_count,
                    damage_event_count, last_scan_at_utc, adapter_state,
                    created_at_utc
             from catalog_units{where_clause}
             order by origin_kind, created_at_utc, unit_id"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|err| sqlite_error("prepare catalog unit stream", err))?;
        let mut rows = if let Some(origin_kind) = origin_kind {
            stmt.query(params![origin_kind])
        } else {
            stmt.query([])
        }
        .map_err(|err| sqlite_error("query catalog unit stream", err))?;
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate catalog unit stream", err))?
        {
            if visit(catalog_unit_from_row(row)?).is_break() {
                break;
            }
        }
        Ok(())
    }

    /// List known tapes from the rebuildable projection.
    pub fn list_tapes(&self, pool_id: Option<&str>) -> Result<Vec<TapeRecord>, StateError> {
        let pool_id = pool_id
            .map(normalize_pool_id)
            .transpose()?
            .filter(|value| !value.is_empty());
        let where_clause = if pool_id.is_some() {
            " where tapes.pool_id = ?1"
        } else {
            ""
        };
        let sql = format!(
            "select tapes.tape_uuid, tapes.voltag, tapes.pool_id,
                    (
                      select objects.body_format
                      from catalog_units
                      join objects on objects.object_id = catalog_units.native_object_id
                      where catalog_units.tape_uuid = tapes.tape_uuid
                        and catalog_units.origin_kind = 'native_object'
                        and objects.body_format is not null
                      group by objects.body_format
                      order by count(*) desc, objects.body_format
                      limit 1
                    ),
                    block_size, scheme_id,
                    data_blocks_per_stripe, parity_blocks_per_stripe,
                    stripes_per_neighborhood, last_committed_tape_file,
                    total_committed_ordinals, state, updated_at_utc
             from tapes{where_clause}
             order by hex(tapes.tape_uuid)"
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|err| sqlite_error("prepare tape query", err))?;
        let mut rows = if let Some(pool_id) = pool_id.as_deref() {
            stmt.query(params![pool_id])
        } else {
            stmt.query([])
        }
        .map_err(|err| sqlite_error("query tapes", err))?;
        let mut tapes = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate tapes", err))?
        {
            tapes.push(tape_from_row(row)?);
        }
        Ok(tapes)
    }

    /// Fetch one known tape by UUID.
    pub fn get_tape(&self, tape_uuid: &[u8]) -> Result<Option<TapeRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select tapes.tape_uuid, tapes.voltag, tapes.pool_id,
                        (
                          select objects.body_format
                          from catalog_units
                          join objects on objects.object_id = catalog_units.native_object_id
                          where catalog_units.tape_uuid = tapes.tape_uuid
                            and catalog_units.origin_kind = 'native_object'
                            and objects.body_format is not null
                          group by objects.body_format
                          order by count(*) desc, objects.body_format
                          limit 1
                        ),
                        block_size, scheme_id,
                        data_blocks_per_stripe, parity_blocks_per_stripe,
                        stripes_per_neighborhood, last_committed_tape_file,
                        total_committed_ordinals, state, updated_at_utc
                 from tapes
                 where tapes.tape_uuid = ?1",
            )
            .map_err(|err| sqlite_error("prepare tape lookup", err))?;
        let mut rows = stmt
            .query(params![tape_uuid])
            .map_err(|err| sqlite_error("query tape lookup", err))?;
        match rows
            .next()
            .map_err(|err| sqlite_error("iterate tape lookup", err))?
        {
            Some(row) => Ok(Some(tape_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Fetch one known tape by operator-facing barcode / volume tag.
    pub fn get_tape_by_voltag(&self, voltag: &str) -> Result<Option<TapeRecord>, StateError> {
        let voltag = voltag.trim();
        if voltag.is_empty() {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare(
                "select tapes.tape_uuid, tapes.voltag, tapes.pool_id,
                        (
                          select objects.body_format
                          from catalog_units
                          join objects on objects.object_id = catalog_units.native_object_id
                          where catalog_units.tape_uuid = tapes.tape_uuid
                            and catalog_units.origin_kind = 'native_object'
                            and objects.body_format is not null
                          group by objects.body_format
                          order by count(*) desc, objects.body_format
                          limit 1
                        ),
                        block_size, scheme_id,
                        data_blocks_per_stripe, parity_blocks_per_stripe,
                        stripes_per_neighborhood, last_committed_tape_file,
                        total_committed_ordinals, state, updated_at_utc
                 from tapes
                 where tapes.voltag = ?1",
            )
            .map_err(|err| sqlite_error("prepare tape voltag lookup", err))?;
        let mut rows = stmt
            .query(params![voltag])
            .map_err(|err| sqlite_error("query tape voltag lookup", err))?;
        match rows
            .next()
            .map_err(|err| sqlite_error("iterate tape voltag lookup", err))?
        {
            Some(row) => Ok(Some(tape_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Distinct pool snapshots on committed object copies for one tape.
    pub fn committed_copy_pool_snapshots(
        &self,
        tape_uuid: &[u8],
    ) -> Result<Vec<Option<String>>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select distinct pool_id
                 from object_copies
                 where tape_uuid = ?1
                   and status = 'committed'
                 order by pool_id is null, pool_id",
            )
            .map_err(|err| sqlite_error("prepare committed-copy pool query", err))?;
        let mut rows = stmt
            .query(params![tape_uuid])
            .map_err(|err| sqlite_error("query committed-copy pools", err))?;
        let mut pools = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate committed-copy pools", err))?
        {
            pools.push(row_get(row, 0, "object_copies.pool_id")?);
        }
        Ok(pools)
    }

    /// List committed tape files for one tape.
    pub fn list_tape_files(&self, tape_uuid: &[u8]) -> Result<Vec<TapeFileRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select tape_uuid, tape_file_number, kind, block_count, object_id
                 from tape_files
                 where tape_uuid = ?1
                 order by tape_file_number",
            )
            .map_err(|err| sqlite_error("prepare tape-file query", err))?;
        let mut rows = stmt
            .query(params![tape_uuid])
            .map_err(|err| sqlite_error("query tape files", err))?;
        let mut files = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate tape files", err))?
        {
            files.push(tape_file_from_row(row)?);
        }
        Ok(files)
    }

    /// Fetch one source-neutral catalog unit by stable id.
    pub fn get_catalog_unit(&self, unit_id: &str) -> Result<Option<CatalogUnitRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select unit_id, tape_uuid, origin_kind, format_id, native_object_id,
                        scan_id, source_kind, source_id, confidence, entry_count,
                        damage_event_count, last_scan_at_utc, adapter_state,
                        created_at_utc
                 from catalog_units
                 where unit_id = ?1",
            )
            .map_err(|err| sqlite_error("prepare catalog unit lookup", err))?;
        let mut rows = stmt
            .query(params![unit_id])
            .map_err(|err| sqlite_error("query catalog unit lookup", err))?;
        match rows
            .next()
            .map_err(|err| sqlite_error("iterate catalog unit lookup", err))?
        {
            Some(row) => Ok(Some(catalog_unit_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// List native objects for the Layer 5 object hot path.
    pub fn list_native_objects(&self) -> Result<Vec<NativeObjectRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select object_id, caller_object_id, body_format, logical_size_bytes,
                        content_hash, metadata_hash, created_at_utc
                 from objects
                 order by created_at_utc, object_id",
            )
            .map_err(|err| sqlite_error("prepare native object query", err))?;
        let mut rows = stmt
            .query([])
            .map_err(|err| sqlite_error("query native objects", err))?;
        let mut objects = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate native objects", err))?
        {
            objects.push(native_object_from_row(row)?);
        }
        drop(rows);
        drop(stmt);

        let positions = objects
            .iter()
            .enumerate()
            .map(|(idx, object)| (object.object_id.clone(), idx))
            .collect::<HashMap<_, _>>();
        for copy in self.list_all_native_object_copies()? {
            if let Some(idx) = positions.get(copy.object_id.as_str()) {
                objects[*idx].copies.push(copy);
            }
        }
        Ok(objects)
    }

    /// Visit native objects in catalog order without materializing the table.
    pub fn for_each_native_object<F>(&self, mut visit: F) -> Result<(), StateError>
    where
        F: FnMut(NativeObjectRecord) -> ControlFlow<()>,
    {
        let mut stmt = self
            .conn
            .prepare(
                "select objects.object_id, objects.caller_object_id,
                        objects.body_format, objects.logical_size_bytes,
                        objects.content_hash, objects.metadata_hash,
                        objects.created_at_utc,
                        object_copies.object_id, object_copies.tape_uuid,
                        object_copies.tape_file_number,
                        object_copies.first_body_lba,
                        object_copies.first_parity_data_ordinal,
                        object_copies.protected_until_ordinal,
                        object_copies.status,
                        object_copies.pool_id,
                        object_copies.representation,
                        object_copies.key_id,
                        object_copies.metadata_frame_len,
                        object_copies.plaintext_digest,
                        object_copies.stored_digest
                 from objects
                 left join object_copies
                   on object_copies.object_id = objects.object_id
                 order by objects.created_at_utc, objects.object_id,
                          hex(object_copies.tape_uuid),
                          object_copies.tape_file_number",
            )
            .map_err(|err| sqlite_error("prepare native object stream", err))?;
        let mut rows = stmt
            .query([])
            .map_err(|err| sqlite_error("query native object stream", err))?;
        let mut current: Option<NativeObjectRecord> = None;
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate native object stream", err))?
        {
            let row_object_id: String = row_get(row, 0, "objects.object_id")?;
            if current
                .as_ref()
                .map(|object| object.object_id.as_str() != row_object_id.as_str())
                .unwrap_or(true)
            {
                if let Some(object) = current.take() {
                    if visit(object).is_break() {
                        return Ok(());
                    }
                }
                current = Some(native_object_from_row(row)?);
            }
            if let Some(copy) = native_object_copy_from_join_row(row, 7)? {
                if let Some(object) = current.as_mut() {
                    object.copies.push(copy);
                }
            }
        }
        if let Some(object) = current.take() {
            let _ = visit(object);
        }
        Ok(())
    }

    /// Fetch a native object and its copies by object id.
    pub fn get_native_object(
        &self,
        object_id: &str,
    ) -> Result<Option<NativeObjectRecord>, StateError> {
        let Some(object) = self.get_native_object_without_copies(object_id)? else {
            return Ok(None);
        };
        self.attach_native_object_copies(object).map(Some)
    }

    /// Fetch one native object member-file row by object id and file id.
    pub fn get_native_object_file(
        &self,
        object_id: &str,
        file_id: &str,
    ) -> Result<Option<NativeObjectFileRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select object_id, file_id, path, size_bytes, file_sha256,
                        first_chunk_lba, chunk_count, mtime, executable
                 from object_files
                 where object_id = ?1 and file_id = ?2",
            )
            .map_err(|err| sqlite_error("prepare native object file lookup", err))?;
        let mut rows = stmt
            .query(params![object_id, file_id])
            .map_err(|err| sqlite_error("query native object file lookup", err))?;
        match rows
            .next()
            .map_err(|err| sqlite_error("iterate native object file lookup", err))?
        {
            Some(row) => native_object_file_from_row(row).map(Some),
            None => Ok(None),
        }
    }

    /// List native object member-file rows.
    pub fn list_native_object_files(
        &self,
        object_id: &str,
    ) -> Result<Vec<NativeObjectFileRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select object_id, file_id, path, size_bytes, file_sha256,
                        first_chunk_lba, chunk_count, mtime, executable
                 from object_files
                 where object_id = ?1
                 order by path, file_id",
            )
            .map_err(|err| sqlite_error("prepare native object file query", err))?;
        let mut rows = stmt
            .query(params![object_id])
            .map_err(|err| sqlite_error("query native object files", err))?;
        let mut files = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate native object files", err))?
        {
            files.push(native_object_file_from_row(row)?);
        }
        Ok(files)
    }

    /// Fetch a native object and its copies by content hash.
    pub fn get_native_object_by_content_hash(
        &self,
        content_hash: &[u8],
    ) -> Result<Option<NativeObjectRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select object_id, caller_object_id, body_format, logical_size_bytes,
                        content_hash, metadata_hash, created_at_utc
                 from objects
                 where content_hash = ?1
                 order by created_at_utc, object_id
                 limit 1",
            )
            .map_err(|err| sqlite_error("prepare native object content-hash lookup", err))?;
        let mut rows = stmt
            .query(params![content_hash])
            .map_err(|err| sqlite_error("query native object content-hash lookup", err))?;
        let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate native object content-hash lookup", err))?
        else {
            return Ok(None);
        };
        let mut object = native_object_from_row(row)?;
        drop(rows);
        drop(stmt);
        object = self.attach_native_object_copies(object)?;
        Ok(Some(object))
    }

    /// Fetch a native object and its copies by caller-supplied object id.
    pub fn get_native_object_by_caller_object_id(
        &self,
        caller_object_id: &str,
    ) -> Result<Option<NativeObjectRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select object_id, caller_object_id, body_format, logical_size_bytes,
                        content_hash, metadata_hash, created_at_utc
                 from objects
                 where caller_object_id = ?1
                 order by created_at_utc, object_id
                 limit 1",
            )
            .map_err(|err| sqlite_error("prepare native object caller-id lookup", err))?;
        let mut rows = stmt
            .query(params![caller_object_id])
            .map_err(|err| sqlite_error("query native object caller-id lookup", err))?;
        let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate native object caller-id lookup", err))?
        else {
            return Ok(None);
        };
        let mut object = native_object_from_row(row)?;
        drop(rows);
        drop(stmt);
        object = self.attach_native_object_copies(object)?;
        Ok(Some(object))
    }

    /// Return concrete tape copies for a native object.
    pub fn find_native_object_copies(
        &self,
        object_id: &str,
    ) -> Result<Vec<NativeObjectCopyRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select object_id, tape_uuid, tape_file_number,
                        first_body_lba, first_parity_data_ordinal,
                        protected_until_ordinal, status, pool_id,
                        representation, key_id, metadata_frame_len,
                        plaintext_digest, stored_digest
                 from object_copies
                 where object_id = ?1
                 order by hex(tape_uuid), tape_file_number",
            )
            .map_err(|err| sqlite_error("prepare native object copy query", err))?;
        let mut rows = stmt
            .query(params![object_id])
            .map_err(|err| sqlite_error("query native object copies", err))?;
        let mut copies = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate native object copies", err))?
        {
            copies.push(native_object_copy_from_row(row)?);
        }
        Ok(copies)
    }

    fn get_native_object_without_copies(
        &self,
        object_id: &str,
    ) -> Result<Option<NativeObjectRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select object_id, caller_object_id, body_format, logical_size_bytes,
                        content_hash, metadata_hash, created_at_utc
                 from objects
                 where object_id = ?1",
            )
            .map_err(|err| sqlite_error("prepare native object lookup", err))?;
        let mut rows = stmt
            .query(params![object_id])
            .map_err(|err| sqlite_error("query native object lookup", err))?;
        let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate native object lookup", err))?
        else {
            return Ok(None);
        };
        Ok(Some(native_object_from_row(row)?))
    }

    fn attach_native_object_copies(
        &self,
        mut object: NativeObjectRecord,
    ) -> Result<NativeObjectRecord, StateError> {
        object.copies = self.find_native_object_copies(object.object_id.as_str())?;
        Ok(object)
    }

    fn list_all_native_object_copies(&self) -> Result<Vec<NativeObjectCopyRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select object_id, tape_uuid, tape_file_number,
                        first_body_lba, first_parity_data_ordinal,
                        protected_until_ordinal, status, pool_id,
                        representation, key_id, metadata_frame_len,
                        plaintext_digest, stored_digest
                 from object_copies
                 order by object_id, hex(tape_uuid), tape_file_number",
            )
            .map_err(|err| sqlite_error("prepare all native object copy query", err))?;
        let mut rows = stmt
            .query([])
            .map_err(|err| sqlite_error("query all native object copies", err))?;
        let mut copies = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate all native object copies", err))?
        {
            copies.push(native_object_copy_from_row(row)?);
        }
        Ok(copies)
    }

    /// Fetch one projected operation by UUID text.
    pub fn get_operation(&self, operation_id: &str) -> Result<Option<OperationRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select operation_id, operation_kind, state, session_id, subject,
                        started_at_utc, updated_at_utc
                 from operations
                 where operation_id = ?1",
            )
            .map_err(|err| sqlite_error("prepare operation lookup", err))?;
        let mut rows = stmt
            .query(params![operation_id])
            .map_err(|err| sqlite_error("query operation lookup", err))?;
        match rows
            .next()
            .map_err(|err| sqlite_error("iterate operation lookup", err))?
        {
            Some(row) => Ok(Some(operation_from_row(row)?)),
            None => Ok(None),
        }
    }

    /// List projected operations in most-recent update order.
    pub fn list_operations(&self) -> Result<Vec<OperationRecord>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select operation_id, operation_kind, state, session_id, subject,
                        started_at_utc, updated_at_utc
                 from operations
                 order by updated_at_utc desc, operation_id",
            )
            .map_err(|err| sqlite_error("prepare operation listing", err))?;
        let mut rows = stmt
            .query([])
            .map_err(|err| sqlite_error("query operation listing", err))?;
        let mut operations = Vec::new();
        while let Some(row) = rows
            .next()
            .map_err(|err| sqlite_error("iterate operation listing", err))?
        {
            operations.push(operation_from_row(row)?);
        }
        Ok(operations)
    }

    /// Return operations that are not terminal after audit replay.
    pub fn non_terminal_operations(&self) -> Result<Vec<RestartOperation>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select operation_id, operation_kind, session_id, subject,
                        (
                          select idempotency_key from idempotency_keys
                          where idempotency_keys.operation_id = operations.operation_id
                            and terminal_state is null
                          order by actor_fingerprint, idempotency_key
                          limit 1
                        ),
                        (
                          select actor_fingerprint from idempotency_keys
                          where idempotency_keys.operation_id = operations.operation_id
                            and terminal_state is null
                          order by actor_fingerprint, idempotency_key
                          limit 1
                        )
                 from operations
                 where state in ('queued', 'running', 'cancel_requested')
                 order by started_at_utc, operation_id",
            )
            .map_err(|err| sqlite_error("prepare non-terminal operation query", err))?;
        let rows = stmt
            .query_map([], |row| {
                let operation_id: String = row.get(0)?;
                let session_id: Option<String> = row.get(2)?;
                let idempotency_key: Option<String> = row.get(4)?;
                Ok((
                    operation_id,
                    row.get(1)?,
                    session_id,
                    row.get(3)?,
                    idempotency_key,
                    row.get(5)?,
                ))
            })
            .map_err(|err| sqlite_error("query non-terminal operations", err))?;
        let mut out = Vec::new();
        for row in rows {
            let (
                operation_id,
                operation_kind,
                session_id,
                subject,
                idempotency_key,
                actor_fingerprint,
            ): (
                String,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            ) = row.map_err(|err| sqlite_error("read non-terminal operation row", err))?;
            out.push(RestartOperation {
                operation_id: parse_uuid_for_index(&operation_id, "operation_id")?,
                operation_kind,
                session_id: session_id
                    .map(|value| parse_uuid_for_index(&value, "session_id"))
                    .transpose()?,
                idempotency_key: idempotency_key
                    .map(|value| parse_uuid_for_index(&value, "idempotency_key"))
                    .transpose()?,
                actor_fingerprint,
                subject,
            });
        }
        Ok(out)
    }

    /// Return sessions that are not terminal after audit replay.
    pub fn non_terminal_sessions(&self) -> Result<Vec<RestartSession>, StateError> {
        let mut stmt = self
            .conn
            .prepare(
                "select session_id, session_kind, tape_uuid, library_serial, drive_bay
                 from sessions
                 where state = 'open'
                 order by opened_at_utc, session_id",
            )
            .map_err(|err| sqlite_error("prepare non-terminal session query", err))?;
        let rows = stmt
            .query_map([], |row| {
                let session_id: String = row.get(0)?;
                Ok((
                    session_id,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })
            .map_err(|err| sqlite_error("query non-terminal sessions", err))?;
        let mut out = Vec::new();
        for row in rows {
            let (session_id, session_kind, tape_uuid, library_serial, drive_bay) =
                row.map_err(|err| sqlite_error("read non-terminal session row", err))?;
            out.push(RestartSession {
                session_id: parse_uuid_for_index(&session_id, "session_id")?,
                session_kind,
                tape_uuid,
                library_serial,
                drive_bay,
            });
        }
        Ok(out)
    }

    /// Return the projected operation state for typed callers and tests.
    pub fn operation_state(&self, operation_id: Uuid) -> Result<Option<String>, StateError> {
        self.conn
            .query_row(
                "select state from operations where operation_id = ?1",
                params![operation_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| sqlite_error("read operation state", err))
    }

    /// Return the projected session state for typed callers and tests.
    pub fn session_state(&self, session_id: Uuid) -> Result<Option<String>, StateError> {
        self.conn
            .query_row(
                "select state from sessions where session_id = ?1",
                params![session_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| sqlite_error("read session state", err))
    }

    /// Return the projected idempotency terminal state for typed callers/tests.
    pub fn idempotency_terminal_state(
        &self,
        actor_fingerprint: &str,
        idempotency_key: Uuid,
    ) -> Result<Option<String>, StateError> {
        self.conn
            .query_row(
                "select terminal_state from idempotency_keys
                 where actor_fingerprint = ?1 and idempotency_key = ?2",
                params![actor_fingerprint, idempotency_key.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|err| sqlite_error("read idempotency terminal state", err))
            .map(|value| value.flatten())
    }
}

fn parse_uuid_for_index(value: &str, field: &str) -> Result<Uuid, StateError> {
    Uuid::parse_str(value)
        .map_err(|err| StateError::IndexCorrupt(format!("{field} is not a uuid: {err}")))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProvisionTapeGeometry {
    block_size: i64,
    scheme_id: Option<String>,
    data_blocks_per_stripe: Option<i64>,
    parity_blocks_per_stripe: Option<i64>,
    stripes_per_neighborhood: Option<i64>,
}

impl ProvisionTapeGeometry {
    fn from_parity(block_size: u32, parity: &ParityConfig) -> Result<Self, StateError> {
        match parity {
            ParityConfig::None => Ok(Self {
                block_size: i64::from(block_size),
                scheme_id: None,
                data_blocks_per_stripe: None,
                parity_blocks_per_stripe: None,
                stripes_per_neighborhood: None,
            }),
            ParityConfig::Scheme(scheme) => {
                scheme.validate().map_err(|err| {
                    StateError::ConfigInvalid(format!("invalid tape parity geometry: {err}"))
                })?;
                Ok(Self {
                    block_size: i64::from(block_size),
                    scheme_id: Some(scheme.id.as_str().to_string()),
                    data_blocks_per_stripe: Some(i64::from(scheme.data_blocks_per_stripe)),
                    parity_blocks_per_stripe: Some(i64::from(scheme.parity_blocks_per_stripe)),
                    stripes_per_neighborhood: Some(i64::from(scheme.stripes_per_neighborhood)),
                })
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExistingProvisionedTape {
    tape_uuid: Vec<u8>,
    voltag: Option<String>,
    geometry: ProvisionTapeGeometry,
    last_committed_tape_file: Option<i64>,
    state: String,
}

#[derive(Clone, Debug)]
struct PreservedTapeRow {
    tape_uuid: Vec<u8>,
    voltag: Option<String>,
    pool_id: Option<String>,
    block_size: Option<i64>,
    scheme_id: Option<String>,
    data_blocks_per_stripe: Option<i64>,
    parity_blocks_per_stripe: Option<i64>,
    stripes_per_neighborhood: Option<i64>,
    highest_protected_ordinal: i64,
    total_committed_ordinals: i64,
    last_committed_tape_file: Option<i64>,
    state: String,
    updated_at_utc: String,
}

impl ExistingProvisionedTape {
    fn is_unwritten(&self) -> bool {
        self.last_committed_tape_file.is_none()
    }
}

fn provision_tape_tx(
    tx: &rusqlite::Transaction<'_>,
    tape_uuid: [u8; 16],
    voltag: &str,
    geometry: &ProvisionTapeGeometry,
    force: bool,
    updated_at: &str,
) -> Result<(), StateError> {
    let existing = find_provisioned_tape_tx(tx, tape_uuid, voltag)?;
    if let Some(existing) = existing {
        // Retirement is terminal, including against `force`: `force` reuses
        // the *row* (the `reprovision_tape_tx` escape hatch below), and a
        // retired identity's history — its journals, its audit trail — must
        // stay attached to its uuid forever. The recycle path provisions a
        // fresh uuid instead, which never resolves to this row because the
        // retired row's voltag was detached at retire time.
        if existing.state == "retired" {
            return Err(StateError::TapeProvisionConflict(format!(
                "tape {} is retired; retired identities are permanent — \
                 init the medium under a fresh identity",
                hex_uuid_from_slice(existing.tape_uuid.as_slice())
            )));
        }
        let same_uuid = existing.tape_uuid == tape_uuid.to_vec();
        let same_geometry = existing.geometry == *geometry;
        let same_voltag = existing.voltag.as_deref() == Some(voltag);
        if same_uuid && same_geometry && same_voltag {
            return Ok(());
        }
        if same_uuid && same_geometry {
            return update_provisioned_tape_voltag_tx(
                tx,
                existing.tape_uuid.as_slice(),
                voltag,
                updated_at,
            );
        }
        if (!same_uuid || !same_geometry) && !existing.is_unwritten() && !force {
            return Err(StateError::TapeProvisionConflict(format!(
                "tape {} is already written; pass force to replace UUID or geometry",
                hex_uuid_from_slice(existing.tape_uuid.as_slice())
            )));
        }
        reprovision_tape_tx(
            tx,
            existing.tape_uuid.as_slice(),
            tape_uuid,
            voltag,
            geometry,
            !existing.is_unwritten(),
            updated_at,
        )
    } else {
        insert_provisioned_tape_tx(tx, tape_uuid, voltag, geometry, updated_at)
    }
}

fn find_provisioned_tape_tx(
    tx: &rusqlite::Transaction<'_>,
    tape_uuid: [u8; 16],
    voltag: &str,
) -> Result<Option<ExistingProvisionedTape>, StateError> {
    let by_uuid = query_provisioned_tape_tx(
        tx,
        "select tape_uuid, voltag, block_size, scheme_id, data_blocks_per_stripe,
                parity_blocks_per_stripe, stripes_per_neighborhood,
                last_committed_tape_file, state
         from tapes
         where tape_uuid = ?1",
        params![tape_uuid.to_vec()],
    )?;
    if by_uuid.is_some() {
        return Ok(by_uuid);
    }
    query_provisioned_tape_tx(
        tx,
        "select tape_uuid, voltag, block_size, scheme_id, data_blocks_per_stripe,
                parity_blocks_per_stripe, stripes_per_neighborhood,
                last_committed_tape_file, state
         from tapes
         where voltag = ?1
         order by hex(tape_uuid)
         limit 1",
        params![voltag],
    )
}

fn query_provisioned_tape_tx<P>(
    tx: &rusqlite::Transaction<'_>,
    sql: &str,
    params: P,
) -> Result<Option<ExistingProvisionedTape>, StateError>
where
    P: rusqlite::Params,
{
    tx.query_row(sql, params, |row| {
        Ok(ExistingProvisionedTape {
            tape_uuid: row.get(0)?,
            voltag: row.get(1)?,
            geometry: ProvisionTapeGeometry {
                block_size: row.get(2)?,
                scheme_id: row.get(3)?,
                data_blocks_per_stripe: row.get(4)?,
                parity_blocks_per_stripe: row.get(5)?,
                stripes_per_neighborhood: row.get(6)?,
            },
            last_committed_tape_file: row.get(7)?,
            state: row.get(8)?,
        })
    })
    .optional()
    .map_err(|err| sqlite_error("query existing provisioned tape", err))
}

fn insert_provisioned_tape_tx(
    tx: &rusqlite::Transaction<'_>,
    tape_uuid: [u8; 16],
    voltag: &str,
    geometry: &ProvisionTapeGeometry,
    updated_at: &str,
) -> Result<(), StateError> {
    tx.execute(
        "insert into tapes(
           tape_uuid, voltag, block_size, scheme_id, data_blocks_per_stripe,
           parity_blocks_per_stripe, stripes_per_neighborhood,
           highest_protected_ordinal, total_committed_ordinals,
           last_committed_tape_file, state, updated_at_utc
         )
         values(?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, null, 'ready', ?8)",
        params![
            tape_uuid.to_vec(),
            voltag,
            geometry.block_size,
            geometry.scheme_id.as_deref(),
            geometry.data_blocks_per_stripe,
            geometry.parity_blocks_per_stripe,
            geometry.stripes_per_neighborhood,
            updated_at,
        ],
    )
    .map_err(|err| sqlite_error("insert provisioned tape", err))?;
    Ok(())
}

fn update_provisioned_tape_voltag_tx(
    tx: &rusqlite::Transaction<'_>,
    tape_uuid: &[u8],
    voltag: &str,
    updated_at: &str,
) -> Result<(), StateError> {
    tx.execute(
        "update tapes
         set voltag = ?2,
             updated_at_utc = ?3
         where tape_uuid = ?1",
        params![tape_uuid, voltag, updated_at],
    )
    .map_err(|err| sqlite_error("update provisioned tape voltag", err))?;
    Ok(())
}

fn reprovision_tape_tx(
    tx: &rusqlite::Transaction<'_>,
    old_tape_uuid: &[u8],
    new_tape_uuid: [u8; 16],
    voltag: &str,
    geometry: &ProvisionTapeGeometry,
    clear_committed_rows: bool,
    updated_at: &str,
) -> Result<(), StateError> {
    tx.execute(
        "update tapes
         set tape_uuid = ?2,
             voltag = ?3,
             block_size = ?4,
             scheme_id = ?5,
             data_blocks_per_stripe = ?6,
             parity_blocks_per_stripe = ?7,
             stripes_per_neighborhood = ?8,
             state = 'ready',
             highest_protected_ordinal = 0,
             total_committed_ordinals = 0,
             last_committed_tape_file = null,
             updated_at_utc = ?9
         where tape_uuid = ?1",
        params![
            old_tape_uuid,
            new_tape_uuid.to_vec(),
            voltag,
            geometry.block_size,
            geometry.scheme_id.as_deref(),
            geometry.data_blocks_per_stripe,
            geometry.parity_blocks_per_stripe,
            geometry.stripes_per_neighborhood,
            updated_at,
        ],
    )
    .map_err(|err| sqlite_error("re-provision tape", err))?;
    if clear_committed_rows {
        tx.execute(
            "delete from tape_files where tape_uuid = ?1",
            params![old_tape_uuid],
        )
        .map_err(|err| sqlite_error("clear stale tape_files after re-provision", err))?;
        tx.execute(
            "delete from object_copies where tape_uuid = ?1",
            params![old_tape_uuid],
        )
        .map_err(|err| sqlite_error("clear stale object_copies after re-provision", err))?;
    }
    Ok(())
}

fn clear_rebuildable_tables(tx: &rusqlite::Transaction<'_>) -> Result<(), StateError> {
    for table in [
        "catalog_units",
        "object_copies",
        "tape_files",
        "objects",
        "tapes",
        "idempotency_keys",
        "operations",
        "sessions",
        "ingested_sources",
    ] {
        tx.execute(&format!("delete from {table}"), [])
            .map_err(|err| sqlite_error("clear rebuildable projection table", err))?;
    }
    Ok(())
}

fn query_preserved_tape_rows_tx(
    tx: &rusqlite::Transaction<'_>,
) -> Result<Vec<PreservedTapeRow>, StateError> {
    let mut stmt = tx
        .prepare(
            "select tape_uuid, voltag, pool_id, block_size, scheme_id,
                    data_blocks_per_stripe, parity_blocks_per_stripe,
                    stripes_per_neighborhood, highest_protected_ordinal,
                    total_committed_ordinals, last_committed_tape_file,
                    state, updated_at_utc
             from tapes
             where voltag is not null
                or pool_id is not null
                or state in ('ready', 'sealed', 'retired')",
        )
        .map_err(|err| sqlite_error("prepare preserved tape query", err))?;
    let mut rows = stmt
        .query([])
        .map_err(|err| sqlite_error("query preserved tapes", err))?;
    let mut preserved = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|err| sqlite_error("iterate preserved tapes", err))?
    {
        preserved.push(PreservedTapeRow {
            tape_uuid: row_get(row, 0, "tapes.tape_uuid")?,
            voltag: row_get(row, 1, "tapes.voltag")?,
            pool_id: row_get(row, 2, "tapes.pool_id")?,
            block_size: row_get(row, 3, "tapes.block_size")?,
            scheme_id: row_get(row, 4, "tapes.scheme_id")?,
            data_blocks_per_stripe: row_get(row, 5, "tapes.data_blocks_per_stripe")?,
            parity_blocks_per_stripe: row_get(row, 6, "tapes.parity_blocks_per_stripe")?,
            stripes_per_neighborhood: row_get(row, 7, "tapes.stripes_per_neighborhood")?,
            highest_protected_ordinal: row_get(row, 8, "tapes.highest_protected_ordinal")?,
            total_committed_ordinals: row_get(row, 9, "tapes.total_committed_ordinals")?,
            last_committed_tape_file: row_get(row, 10, "tapes.last_committed_tape_file")?,
            state: row_get(row, 11, "tapes.state")?,
            updated_at_utc: row_get(row, 12, "tapes.updated_at_utc")?,
        });
    }
    Ok(preserved)
}

fn restore_preserved_tape_rows_tx(
    tx: &rusqlite::Transaction<'_>,
    rows: &[PreservedTapeRow],
) -> Result<(), StateError> {
    for row in rows {
        tx.execute(
            "insert into tapes(
               tape_uuid, voltag, pool_id, block_size, scheme_id,
               data_blocks_per_stripe, parity_blocks_per_stripe,
               stripes_per_neighborhood, highest_protected_ordinal,
               total_committed_ordinals, last_committed_tape_file,
               state, updated_at_utc
            )
             values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                row.tape_uuid.as_slice(),
                row.voltag.as_deref(),
                row.pool_id.as_deref(),
                row.block_size,
                row.scheme_id.as_deref(),
                row.data_blocks_per_stripe,
                row.parity_blocks_per_stripe,
                row.stripes_per_neighborhood,
                row.highest_protected_ordinal,
                row.total_committed_ordinals,
                row.last_committed_tape_file,
                row.state.as_str(),
                row.updated_at_utc.as_str(),
            ],
        )
        .map_err(|err| sqlite_error("restore preserved tape row", err))?;
    }
    Ok(())
}

fn merge_preserved_tape_operator_columns_tx(
    tx: &rusqlite::Transaction<'_>,
    rows: &[PreservedTapeRow],
) -> Result<(), StateError> {
    for row in rows {
        // `retired` is re-applied over the journal-derived row exactly like
        // `ready`/`sealed`: the 3c journal is authoritative history ("these
        // objects were committed to identity X") and must stay on disk, so
        // without this merge a rebuild would re-ingest it and resurrect the
        // retired identity as `ingested`.
        if row.voltag.is_none()
            && row.pool_id.is_none()
            && !matches!(row.state.as_str(), "ready" | "sealed" | "retired")
        {
            continue;
        }
        tx.execute(
            "update tapes
             set voltag = coalesce(?2, voltag),
                 pool_id = coalesce(?3, pool_id),
                 state =
                   case
                     when ?4 in ('ready', 'sealed', 'retired') then ?4
                     else state
                   end
             where tape_uuid = ?1",
            params![
                row.tape_uuid.as_slice(),
                row.voltag.as_deref(),
                row.pool_id.as_deref(),
                row.state.as_str(),
            ],
        )
        .map_err(|err| sqlite_error("merge preserved tape operator columns", err))?;
    }
    Ok(())
}

fn index_committed_tape_journal_tx(
    tx: &rusqlite::Transaction<'_>,
    input: &TapeJournalIndexInput,
    state: &CommittedState,
    updated_at: &str,
) -> Result<TapeJournalIndexReport, StateError> {
    let scheme = input.scheme.as_ref();
    let last_committed_tape_file = state
        .entries
        .iter()
        .map(|entry| entry.tape_file_number)
        .max()
        .map(i64::from);
    tx.execute(
        "insert into tapes(
           tape_uuid, block_size, scheme_id, data_blocks_per_stripe,
           parity_blocks_per_stripe, stripes_per_neighborhood,
           highest_protected_ordinal, total_committed_ordinals,
           last_committed_tape_file, state, updated_at_utc
         )
         values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'ingested', ?10)
         on conflict(tape_uuid) do update set
           block_size = excluded.block_size,
           scheme_id = excluded.scheme_id,
           data_blocks_per_stripe = excluded.data_blocks_per_stripe,
           parity_blocks_per_stripe = excluded.parity_blocks_per_stripe,
           stripes_per_neighborhood = excluded.stripes_per_neighborhood,
           highest_protected_ordinal = excluded.highest_protected_ordinal,
           total_committed_ordinals = excluded.total_committed_ordinals,
           last_committed_tape_file = excluded.last_committed_tape_file,
           state =
             case
               when tapes.state in ('sealed', 'retired') then tapes.state
               else excluded.state
             end,
           updated_at_utc = excluded.updated_at_utc",
        params![
            input.tape_uuid.to_vec(),
            i64::from(input.block_size),
            scheme.map(|scheme| scheme.id.as_str()),
            scheme.map(|scheme| i64::from(scheme.data_blocks_per_stripe)),
            scheme.map(|scheme| i64::from(scheme.parity_blocks_per_stripe)),
            scheme.map(|scheme| i64::from(scheme.stripes_per_neighborhood)),
            u64_to_i64(state.highest_protected_ordinal, "highest_protected_ordinal")?,
            u64_to_i64(state.total_committed_ordinals, "total_committed_ordinals")?,
            last_committed_tape_file,
            updated_at,
        ],
    )
    .map_err(|err| sqlite_error("upsert tape projection", err))?;

    tx.execute(
        "delete from tape_files where tape_uuid = ?1",
        params![input.tape_uuid.to_vec()],
    )
    .map_err(|err| sqlite_error("clear tape_files projection", err))?;
    tx.execute(
        "delete from object_copies where tape_uuid = ?1",
        params![input.tape_uuid.to_vec()],
    )
    .map_err(|err| sqlite_error("clear object_copies projection", err))?;
    tx.execute(
        "delete from catalog_units where origin_kind = 'native_object' and tape_uuid = ?1",
        params![input.tape_uuid.to_vec()],
    )
    .map_err(|err| sqlite_error("clear native catalog unit projection", err))?;

    let mut object_copies = 0u64;
    for entry in &state.entries {
        insert_tape_file(tx, input.tape_uuid, entry)?;
        if entry.kind == TapeFileKind::Object {
            if let Some(object_id) = entry.object_id.as_ref() {
                let envelope = object_copy_envelope_from_tape_entry(entry)?;
                insert_object_copy_projection_tx(
                    tx,
                    ObjectCopyProjectionRow {
                        object_id,
                        tape_uuid: input.tape_uuid,
                        tape_file_number: entry.tape_file_number,
                        first_body_lba: 0,
                        first_parity_data_ordinal: entry.first_parity_data_ordinal,
                        protected_until_ordinal: entry
                            .first_parity_data_ordinal
                            .map(|_| state.highest_protected_ordinal),
                        status: "committed",
                        representation: envelope.representation,
                        key_id: envelope.key_id,
                        metadata_frame_len: envelope.metadata_frame_len,
                        plaintext_digest: None,
                        stored_digest: None,
                    },
                )?;
                let format_id = native_object_format_id_tx(tx, object_id)?;
                insert_native_catalog_unit_tx(
                    tx,
                    object_id,
                    input.tape_uuid,
                    entry.tape_file_number,
                    format_id.as_str(),
                    updated_at,
                )?;
                object_copies += 1;
            }
        }
    }

    tx.execute(
        "insert into ingested_sources(
           source_kind, source_id, offset_bytes, terminal_hash, updated_at_utc
         )
         values('tape_journal', ?1, ?2, null, ?3)
         on conflict(source_kind, source_id) do update set
           offset_bytes = excluded.offset_bytes,
           terminal_hash = excluded.terminal_hash,
           updated_at_utc = excluded.updated_at_utc",
        params![
            hex_uuid(input.tape_uuid),
            u64_to_i64(input.journal_offset_bytes, "journal_offset_bytes")?,
            updated_at,
        ],
    )
    .map_err(|err| sqlite_error("upsert ingested source", err))?;

    Ok(TapeJournalIndexReport {
        ingestion_pending: false,
        tape_files_rebuilt: state.entries.len() as u64,
        object_copies_rebuilt: object_copies,
    })
}

fn upsert_native_object_projection_tx(
    tx: &rusqlite::Transaction<'_>,
    object: &NativeObjectProjectionInput,
    files: Option<&[NativeObjectFileProjectionInput]>,
    copies: &[NativeObjectCopyProjectionInput],
    created_at_utc: &str,
) -> Result<(), StateError> {
    let logical_size_bytes = opt_u64_to_i64(object.logical_size_bytes, "logical_size_bytes")?;
    tx.execute(
        "insert into objects(
           object_id, caller_object_id, body_format, logical_size_bytes,
           content_hash, metadata_hash, created_at_utc
         )
         values(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         on conflict(object_id) do update set
           caller_object_id = excluded.caller_object_id,
           body_format = excluded.body_format,
           logical_size_bytes = excluded.logical_size_bytes,
           content_hash = excluded.content_hash,
           metadata_hash = excluded.metadata_hash",
        params![
            object.object_id.as_str(),
            object.caller_object_id.as_deref(),
            object.body_format.as_str(),
            logical_size_bytes,
            object.content_hash.as_deref(),
            object.metadata_hash.as_deref(),
            created_at_utc,
        ],
    )
    .map_err(|err| sqlite_error("upsert native object projection", err))?;
    tx.execute(
        "update catalog_units
         set format_id = ?2
         where origin_kind = 'native_object' and native_object_id = ?1",
        params![object.object_id.as_str(), object.body_format.as_str()],
    )
    .map_err(|err| sqlite_error("refresh native catalog unit format", err))?;

    if let Some(files) = files {
        replace_native_object_files_tx(tx, object.object_id.as_str(), files)?;
    }

    for copy in copies {
        if copy.object_id != object.object_id {
            return Err(StateError::IndexMigrationFailed(format!(
                "object copy {} does not match projected object {}",
                copy.object_id, object.object_id
            )));
        }
        insert_object_copy_projection_tx(
            tx,
            ObjectCopyProjectionRow {
                object_id: copy.object_id.as_str(),
                tape_uuid: copy.tape_uuid,
                tape_file_number: copy.tape_file_number,
                first_body_lba: copy.first_body_lba,
                first_parity_data_ordinal: copy.first_parity_data_ordinal,
                protected_until_ordinal: copy.protected_until_ordinal,
                status: copy.status.as_str(),
                representation: Some(copy.representation.as_str()),
                key_id: copy.key_id.as_deref(),
                metadata_frame_len: copy.metadata_frame_len,
                plaintext_digest: copy.plaintext_digest.as_deref(),
                stored_digest: copy.stored_digest.as_deref(),
            },
        )?;
        insert_native_catalog_unit_tx(
            tx,
            copy.object_id.as_str(),
            copy.tape_uuid,
            copy.tape_file_number,
            object.body_format.as_str(),
            created_at_utc,
        )?;
    }

    Ok(())
}

fn replace_native_object_files_tx(
    tx: &rusqlite::Transaction<'_>,
    object_id: &str,
    files: &[NativeObjectFileProjectionInput],
) -> Result<(), StateError> {
    tx.execute(
        "delete from object_files where object_id = ?1",
        params![object_id],
    )
    .map_err(|err| sqlite_error("clear native object file projection", err))?;
    for file in files {
        if file.object_id != object_id {
            return Err(StateError::IndexMigrationFailed(format!(
                "object file {} does not match projected object {object_id}",
                file.object_id
            )));
        }
        insert_native_object_file_projection_tx(tx, file)?;
    }
    Ok(())
}

fn insert_native_object_file_projection_tx(
    tx: &rusqlite::Transaction<'_>,
    file: &NativeObjectFileProjectionInput,
) -> Result<(), StateError> {
    if file.file_id.is_empty() {
        return Err(StateError::IndexMigrationFailed(
            "object file_id must not be empty".to_string(),
        ));
    }
    if file.path.is_empty() {
        return Err(StateError::IndexMigrationFailed(
            "object file path must not be empty".to_string(),
        ));
    }
    validate_optional_sha256(
        Some(file.file_sha256.as_slice()),
        "object_files.file_sha256",
    )?;
    if file.size_bytes == 0 {
        if file.first_chunk_lba.is_some() || file.chunk_count != 0 {
            return Err(StateError::IndexMigrationFailed(
                "empty object file rows must not carry data coordinates".to_string(),
            ));
        }
    } else if file.first_chunk_lba.is_none() || file.chunk_count == 0 {
        return Err(StateError::IndexMigrationFailed(
            "non-empty object file rows require first_chunk_lba and chunk_count".to_string(),
        ));
    }
    let executable = file
        .executable
        .map(|value| if value { 1_i64 } else { 0_i64 });
    tx.execute(
        "insert into object_files(
           object_id, file_id, path, size_bytes, file_sha256,
           first_chunk_lba, chunk_count, mtime, executable
         )
         values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            file.object_id.as_str(),
            file.file_id.as_str(),
            file.path.as_str(),
            u64_to_i64(file.size_bytes, "object_files.size_bytes")?,
            file.file_sha256.as_slice(),
            opt_u64_to_i64(file.first_chunk_lba, "object_files.first_chunk_lba")?,
            u64_to_i64(file.chunk_count, "object_files.chunk_count")?,
            file.mtime.as_deref(),
            executable,
        ],
    )
    .map_err(|err| sqlite_error("insert native object file projection", err))?;
    Ok(())
}

fn project_committed_tape_file_bundle_tx(
    tx: &rusqlite::Transaction<'_>,
    input: &TapeJournalIndexInput,
    bundle: &CommittedBundle,
    updated_at: &str,
) -> Result<TapeJournalIndexReport, StateError> {
    let scheme = input.scheme.as_ref();
    let last_committed_tape_file = bundle
        .entries
        .iter()
        .map(|entry| entry.tape_file_number)
        .max()
        .map(i64::from);
    // The retired arm of the state CASE below is defense in depth: this live
    // bundle path cannot legitimately fire for a retired tape because pool
    // selection rejects any non-`ready` state before a write session opens.
    tx.execute(
        "insert into tapes(
           tape_uuid, block_size, scheme_id, data_blocks_per_stripe,
           parity_blocks_per_stripe, stripes_per_neighborhood,
           highest_protected_ordinal, total_committed_ordinals,
           last_committed_tape_file, state, updated_at_utc
         )
         values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'ready', ?10)
         on conflict(tape_uuid) do update set
           block_size = excluded.block_size,
           scheme_id = excluded.scheme_id,
           data_blocks_per_stripe = excluded.data_blocks_per_stripe,
           parity_blocks_per_stripe = excluded.parity_blocks_per_stripe,
           stripes_per_neighborhood = excluded.stripes_per_neighborhood,
           highest_protected_ordinal =
             case
               when excluded.highest_protected_ordinal > tapes.highest_protected_ordinal
                 then excluded.highest_protected_ordinal
               else tapes.highest_protected_ordinal
             end,
           total_committed_ordinals =
             case
               when excluded.total_committed_ordinals > tapes.total_committed_ordinals
                 then excluded.total_committed_ordinals
               else tapes.total_committed_ordinals
             end,
           last_committed_tape_file =
             case
               when tapes.last_committed_tape_file is null then excluded.last_committed_tape_file
               when excluded.last_committed_tape_file is null then tapes.last_committed_tape_file
               when excluded.last_committed_tape_file > tapes.last_committed_tape_file
                 then excluded.last_committed_tape_file
               else tapes.last_committed_tape_file
             end,
           state =
             case
               when tapes.state in ('sealed', 'retired') then tapes.state
               else excluded.state
             end,
           updated_at_utc = excluded.updated_at_utc",
        params![
            input.tape_uuid.to_vec(),
            i64::from(input.block_size),
            scheme.map(|scheme| scheme.id.as_str()),
            scheme.map(|scheme| i64::from(scheme.data_blocks_per_stripe)),
            scheme.map(|scheme| i64::from(scheme.parity_blocks_per_stripe)),
            scheme.map(|scheme| i64::from(scheme.stripes_per_neighborhood)),
            u64_to_i64(
                bundle.highest_protected_ordinal,
                "highest_protected_ordinal"
            )?,
            u64_to_i64(bundle.total_committed_ordinals, "total_committed_ordinals")?,
            last_committed_tape_file,
            updated_at,
        ],
    )
    .map_err(|err| sqlite_error("upsert incremental tape projection", err))?;

    let mut object_copies = 0u64;
    for entry in &bundle.entries {
        insert_tape_file(tx, input.tape_uuid, entry)?;
        if entry.kind == TapeFileKind::Object {
            if let Some(object_id) = entry.object_id.as_ref() {
                let envelope = object_copy_envelope_from_tape_entry(entry)?;
                insert_object_copy_projection_tx(
                    tx,
                    ObjectCopyProjectionRow {
                        object_id,
                        tape_uuid: input.tape_uuid,
                        tape_file_number: entry.tape_file_number,
                        first_body_lba: 0,
                        first_parity_data_ordinal: entry.first_parity_data_ordinal,
                        protected_until_ordinal: entry
                            .first_parity_data_ordinal
                            .map(|_| bundle.highest_protected_ordinal),
                        status: "committed",
                        representation: envelope.representation,
                        key_id: envelope.key_id,
                        metadata_frame_len: envelope.metadata_frame_len,
                        plaintext_digest: None,
                        stored_digest: None,
                    },
                )?;
                let format_id = native_object_format_id_tx(tx, object_id)?;
                insert_native_catalog_unit_tx(
                    tx,
                    object_id,
                    input.tape_uuid,
                    entry.tape_file_number,
                    format_id.as_str(),
                    updated_at,
                )?;
                object_copies += 1;
            }
        }
    }

    Ok(TapeJournalIndexReport {
        ingestion_pending: false,
        tape_files_rebuilt: bundle.entries.len() as u64,
        object_copies_rebuilt: object_copies,
    })
}

fn project_operation_record(
    tx: &rusqlite::Transaction<'_>,
    record: &AuditRecord,
) -> Result<(), StateError> {
    let Some(operation_id) = record.operation_id else {
        return Ok(());
    };
    let Some(state) = operation_state_for_event(&record.event) else {
        return Ok(());
    };
    let operation_kind = detail_text(record, "operation_kind")
        .unwrap_or_else(|| {
            if matches!(record.event, AuditEvent::OperationStarted) {
                record.subject.kind.clone()
            } else {
                "unknown".to_string()
            }
        })
        .trim()
        .to_string();
    let operation_kind = if operation_kind.is_empty() {
        "unknown".to_string()
    } else {
        operation_kind
    };
    let subject = subject_projection(record);
    tx.execute(
        "insert into operations(
           operation_id, operation_kind, state, session_id, subject,
           started_at_utc, updated_at_utc
         )
         values(?1, ?2, ?3, ?4, ?5, ?6, ?6)
         on conflict(operation_id) do update set
           operation_kind = case
             when excluded.operation_kind != 'unknown' then excluded.operation_kind
             else operations.operation_kind
           end,
           state = excluded.state,
           session_id = coalesce(excluded.session_id, operations.session_id),
           subject = coalesce(excluded.subject, operations.subject),
           updated_at_utc = excluded.updated_at_utc",
        params![
            operation_id.to_string(),
            operation_kind,
            state,
            record.session_id.map(|uuid| uuid.to_string()),
            subject,
            record.timestamp_utc.as_str(),
        ],
    )
    .map_err(|err| sqlite_error("project audit operation", err))?;
    Ok(())
}

fn project_session_record(
    tx: &rusqlite::Transaction<'_>,
    record: &AuditRecord,
) -> Result<(), StateError> {
    let Some(session_id) = record.session_id else {
        return Ok(());
    };
    let Some(state) = session_state_for_event(&record.event) else {
        return Ok(());
    };
    let session_kind = detail_text(record, "session_kind")
        .unwrap_or_else(|| {
            if matches!(record.event, AuditEvent::SessionOpened) {
                record.subject.kind.clone()
            } else {
                "unknown".to_string()
            }
        })
        .trim()
        .to_string();
    let session_kind = if session_kind.is_empty() {
        "unknown".to_string()
    } else {
        session_kind
    };
    tx.execute(
        "insert into sessions(
           session_id, session_kind, tape_uuid, library_serial, drive_bay,
           state, opened_at_utc, updated_at_utc
         )
         values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         on conflict(session_id) do update set
           session_kind = case
             when excluded.session_kind != 'unknown' then excluded.session_kind
             else sessions.session_kind
           end,
           tape_uuid = coalesce(excluded.tape_uuid, sessions.tape_uuid),
           library_serial = coalesce(excluded.library_serial, sessions.library_serial),
           drive_bay = coalesce(excluded.drive_bay, sessions.drive_bay),
           state = excluded.state,
           updated_at_utc = excluded.updated_at_utc",
        params![
            session_id.to_string(),
            session_kind,
            detail_tape_uuid(record, "tape_uuid"),
            detail_text(record, "library_serial"),
            detail_i64(record, "drive_bay"),
            state,
            record.timestamp_utc.as_str(),
        ],
    )
    .map_err(|err| sqlite_error("project audit session", err))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdempotencyProjectionMode {
    Live,
    Replay,
}

fn project_idempotency_record(
    tx: &rusqlite::Transaction<'_>,
    record: &AuditRecord,
    mode: IdempotencyProjectionMode,
) -> Result<(), StateError> {
    let Some(idempotency_key) = record.idempotency_key else {
        return Ok(());
    };
    let actor_fingerprint = detail_text(record, "actor_fingerprint")
        .unwrap_or_else(|| actor_fingerprint(&record.actor));
    if let Some(request_fingerprint) = detail_bytes(record, "request_fingerprint") {
        let Some(operation_id) = record.operation_id else {
            return Err(StateError::IndexMigrationFailed(
                "idempotency request record is missing operation_id".to_string(),
            ));
        };
        upsert_idempotency_request(
            tx,
            &actor_fingerprint,
            idempotency_key,
            request_fingerprint,
            operation_id,
            &record.timestamp_utc,
            mode,
        )?;
    }

    if let Some(terminal_state) = terminal_state_for_event(&record.event) {
        tx.execute(
            "update idempotency_keys
             set terminal_state = ?1,
                 response_fingerprint = coalesce(?2, response_fingerprint),
                 updated_at_utc = ?3
             where actor_fingerprint = ?4 and idempotency_key = ?5",
            params![
                terminal_state,
                detail_bytes(record, "response_fingerprint"),
                record.timestamp_utc.as_str(),
                actor_fingerprint,
                idempotency_key.to_string(),
            ],
        )
        .map_err(|err| sqlite_error("project idempotency terminal state", err))?;
    }

    Ok(())
}

fn upsert_idempotency_request(
    tx: &rusqlite::Transaction<'_>,
    actor_fingerprint: &str,
    idempotency_key: Uuid,
    request_fingerprint: Vec<u8>,
    operation_id: Uuid,
    updated_at_utc: &str,
    mode: IdempotencyProjectionMode,
) -> Result<(), StateError> {
    let existing: Option<Vec<u8>> = tx
        .query_row(
            "select request_fingerprint from idempotency_keys
             where actor_fingerprint = ?1 and idempotency_key = ?2",
            params![actor_fingerprint, idempotency_key.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| sqlite_error("read existing idempotency request", err))?;
    if let Some(existing) = existing {
        if existing != request_fingerprint {
            return match mode {
                IdempotencyProjectionMode::Live => Err(StateError::IdempotencyConflict(format!(
                    "actor {actor_fingerprint} reused idempotency key {idempotency_key}"
                ))),
                IdempotencyProjectionMode::Replay => Ok(()),
            };
        }
    }

    tx.execute(
        "insert into idempotency_keys(
           actor_fingerprint, idempotency_key, request_fingerprint,
           operation_id, terminal_state, response_fingerprint, updated_at_utc
         )
         values(?1, ?2, ?3, ?4, null, null, ?5)
         on conflict(actor_fingerprint, idempotency_key) do update set
           operation_id = excluded.operation_id,
           updated_at_utc = excluded.updated_at_utc",
        params![
            actor_fingerprint,
            idempotency_key.to_string(),
            request_fingerprint,
            operation_id.to_string(),
            updated_at_utc,
        ],
    )
    .map_err(|err| sqlite_error("project idempotency request", err))?;
    Ok(())
}

fn operation_state_for_event(event: &AuditEvent) -> Option<&'static str> {
    match event {
        AuditEvent::RequestReceived => Some("queued"),
        AuditEvent::OperationStarted => Some("running"),
        AuditEvent::OperationProgress => Some("running"),
        AuditEvent::CancelRequested => Some("cancel_requested"),
        AuditEvent::CancellationRejected => Some("running"),
        AuditEvent::OperationFinished => Some("finished"),
        AuditEvent::OperationFailed => Some("failed"),
        AuditEvent::CancelledBeforeDispatch => Some("cancelled_before_dispatch"),
        AuditEvent::CompletedAfterCancel => Some("completed_after_cancel"),
        AuditEvent::CompletionUnknown => Some("completion_unknown"),
        _ => None,
    }
}

fn terminal_state_for_event(event: &AuditEvent) -> Option<&'static str> {
    match event {
        AuditEvent::OperationFinished => Some("finished"),
        AuditEvent::OperationFailed => Some("failed"),
        AuditEvent::CancelledBeforeDispatch => Some("cancelled_before_dispatch"),
        AuditEvent::CompletedAfterCancel => Some("completed_after_cancel"),
        AuditEvent::CompletionUnknown => Some("completion_unknown"),
        _ => None,
    }
}

fn session_state_for_event(event: &AuditEvent) -> Option<&'static str> {
    match event {
        AuditEvent::SessionOpened => Some("open"),
        AuditEvent::SessionCheckpointed => Some("open"),
        AuditEvent::SessionClosed => Some("closed"),
        AuditEvent::SessionOrphaned => Some("orphaned"),
        AuditEvent::SessionLostByRestart => Some("lost_by_restart"),
        _ => None,
    }
}

fn subject_projection(record: &AuditRecord) -> Option<String> {
    if record.subject.kind.trim().is_empty() {
        return None;
    }
    match record.subject.id.as_deref() {
        Some(id) if !id.trim().is_empty() => Some(format!("{}:{id}", record.subject.kind)),
        _ => Some(record.subject.kind.clone()),
    }
}

fn actor_fingerprint(actor: &AuditActor) -> String {
    match actor {
        AuditActor::System => "system".to_string(),
        AuditActor::User(id) => format!("user:{id}"),
        AuditActor::Service(id) => format!("service:{id}"),
    }
}

fn detail_text(record: &AuditRecord, key: &str) -> Option<String> {
    match record.detail.get(key) {
        Some(ciborium::value::Value::Text(value)) => Some(value.clone()),
        _ => None,
    }
}

fn detail_bytes(record: &AuditRecord, key: &str) -> Option<Vec<u8>> {
    match record.detail.get(key) {
        Some(ciborium::value::Value::Bytes(value)) => Some(value.clone()),
        Some(ciborium::value::Value::Text(value)) => parse_hex(value),
        _ => None,
    }
}

fn detail_i64(record: &AuditRecord, key: &str) -> Option<i64> {
    match record.detail.get(key) {
        Some(ciborium::value::Value::Integer(value)) => {
            let value: i128 = (*value).into();
            i64::try_from(value).ok()
        }
        _ => None,
    }
}

fn detail_tape_uuid(record: &AuditRecord, key: &str) -> Option<Vec<u8>> {
    match record.detail.get(key) {
        Some(ciborium::value::Value::Bytes(value)) if value.len() == 16 => Some(value.clone()),
        Some(ciborium::value::Value::Text(value)) => parse_uuid_bytes(value),
        _ => None,
    }
}

fn parse_uuid_bytes(value: &str) -> Option<Vec<u8>> {
    Uuid::parse_str(value)
        .map(|uuid| uuid.as_bytes().to_vec())
        .ok()
        .or_else(|| {
            let bytes = parse_hex(value)?;
            if bytes.len() == 16 {
                Some(bytes)
            } else {
                None
            }
        })
}

fn parse_hex(value: &str) -> Option<Vec<u8>> {
    let hex = value.trim();
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for idx in (0..hex.len()).step_by(2) {
        out.push(u8::from_str_radix(&hex[idx..idx + 2], 16).ok()?);
    }
    Some(out)
}

fn table_count(tx: &rusqlite::Transaction<'_>, table: &str) -> Result<u64, StateError> {
    let sql = format!("select count(*) from {table}");
    let count = tx
        .query_row(&sql, [], |row| row.get::<_, u64>(0))
        .map_err(|err| sqlite_error("count audit projection rows", err))?;
    Ok(count)
}

fn insert_tape_file(
    tx: &rusqlite::Transaction<'_>,
    tape_uuid: [u8; 16],
    entry: &TapeFileEntry,
) -> Result<(), StateError> {
    tx.execute(
        "insert into tape_files(
           tape_uuid, tape_file_number, kind, block_count, physical_start_hint,
           object_id, first_parity_data_ordinal, epoch_id, protected_ordinal_start,
           protected_ordinal_end_exclusive, canonical_metadata_hash, bundle_uuid, bundle_kind
         )
         values(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, null, null)
         on conflict(tape_uuid, tape_file_number) do update set
           kind = excluded.kind,
           block_count = excluded.block_count,
           physical_start_hint = excluded.physical_start_hint,
           object_id = excluded.object_id,
           first_parity_data_ordinal = excluded.first_parity_data_ordinal,
           epoch_id = excluded.epoch_id,
           protected_ordinal_start = excluded.protected_ordinal_start,
           protected_ordinal_end_exclusive = excluded.protected_ordinal_end_exclusive,
           canonical_metadata_hash = excluded.canonical_metadata_hash,
           bundle_uuid = excluded.bundle_uuid,
           bundle_kind = excluded.bundle_kind",
        params![
            tape_uuid.to_vec(),
            i64::from(entry.tape_file_number),
            tape_file_kind(entry.kind),
            u64_to_i64(entry.block_count, "block_count")?,
            opt_u64_to_i64(entry.physical_start_hint, "physical_start_hint")?,
            entry.object_id.as_deref(),
            opt_u64_to_i64(entry.first_parity_data_ordinal, "first_parity_data_ordinal")?,
            opt_u64_to_i64(entry.epoch_id, "epoch_id")?,
            opt_u64_to_i64(entry.protected_ordinal_start, "protected_ordinal_start")?,
            opt_u64_to_i64(
                entry.protected_ordinal_end_exclusive,
                "protected_ordinal_end_exclusive"
            )?,
            entry.canonical_metadata_hash.map(|hash| hash.to_vec()),
        ],
    )
    .map_err(|err| sqlite_error("insert tape_file projection", err))?;
    Ok(())
}

fn upsert_tape_pool_projection_tx(
    tx: &rusqlite::Transaction<'_>,
    pool_id: &str,
    display_name: Option<&str>,
    copy_class: Option<&str>,
    content_class: Option<&str>,
    created_at_utc: &str,
) -> Result<(), StateError> {
    tx.execute(
        "insert into tape_pools(
           pool_id, display_name, copy_class, content_class, created_at_utc
         )
         values(?1, ?2, ?3, ?4, ?5)
         on conflict(pool_id) do update set
           display_name = excluded.display_name,
           copy_class = excluded.copy_class,
           content_class = excluded.content_class",
        params![
            pool_id,
            display_name,
            copy_class,
            content_class,
            created_at_utc,
        ],
    )
    .map_err(|err| sqlite_error("upsert tape pool projection", err))?;
    Ok(())
}

fn project_tape_pool_membership_tx(
    tx: &rusqlite::Transaction<'_>,
    tape_uuid: [u8; 16],
    pool_id: &str,
) -> Result<(), StateError> {
    let conflicting_pool: Option<Option<String>> = tx
        .query_row(
            "select pool_id
             from object_copies
             where tape_uuid = ?1
               and (pool_id is null or pool_id != ?2)
             order by pool_id is not null, pool_id
             limit 1",
            params![tape_uuid.to_vec(), pool_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| sqlite_error("check tape pool reassignment", err))?;
    if let Some(conflicting_pool) = conflicting_pool {
        let conflicting_pool = conflicting_pool.as_deref().unwrap_or("unassigned");
        return Err(StateError::TapePoolAssignmentConflict(format!(
            "tape {} already has committed copies in pool {conflicting_pool}; cannot assign to {pool_id}",
            hex_uuid(tape_uuid)
        )));
    }
    tx.execute(
        "update tapes set pool_id = ?2 where tape_uuid = ?1",
        params![tape_uuid.to_vec(), pool_id],
    )
    .map_err(|err| sqlite_error("project tape pool membership", err))?;
    Ok(())
}

fn query_memberships_tx(
    tx: &rusqlite::Transaction<'_>,
) -> Result<Vec<(Vec<u8>, String)>, StateError> {
    let mut stmt = tx
        .prepare("select tape_uuid, pool_id from tapes where pool_id is not null")
        .map_err(|err| sqlite_error("prepare tape pool membership reconciliation query", err))?;
    let mut rows = stmt
        .query([])
        .map_err(|err| sqlite_error("query tape pool membership reconciliation", err))?;
    let mut memberships = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|err| sqlite_error("iterate tape pool membership reconciliation", err))?
    {
        memberships.push((
            row_get(row, 0, "tapes.tape_uuid")?,
            row_get(row, 1, "tapes.pool_id")?,
        ));
    }
    Ok(memberships)
}

fn query_tapes_for_pool_derivation_tx(
    tx: &rusqlite::Transaction<'_>,
) -> Result<Vec<([u8; 16], String)>, StateError> {
    let mut stmt = tx
        .prepare("select tape_uuid, voltag from tapes where voltag is not null")
        .map_err(|err| sqlite_error("prepare tape pool rule derivation query", err))?;
    let mut rows = stmt
        .query([])
        .map_err(|err| sqlite_error("query tape pool rule derivation", err))?;
    let mut tapes = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|err| sqlite_error("read tape pool rule derivation row", err))?
    {
        let tape_uuid_bytes: Vec<u8> = row_get(row, 0, "tapes.tape_uuid")?;
        let tape_uuid = tape_uuid_bytes.as_slice().try_into().map_err(|_| {
            StateError::IndexCorrupt(format!(
                "tapes.tape_uuid must be 16 bytes, got {}",
                tape_uuid_bytes.len()
            ))
        })?;
        tapes.push((tape_uuid, row_get(row, 1, "tapes.voltag")?));
    }
    Ok(tapes)
}

fn query_tape_pool_ids_tx(tx: &rusqlite::Transaction<'_>) -> Result<Vec<String>, StateError> {
    let mut stmt = tx
        .prepare("select pool_id from tape_pools")
        .map_err(|err| sqlite_error("prepare tape pool reconciliation query", err))?;
    let mut rows = stmt
        .query([])
        .map_err(|err| sqlite_error("query tape pool reconciliation", err))?;
    let mut pool_ids = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|err| sqlite_error("iterate tape pool reconciliation", err))?
    {
        pool_ids.push(row_get(row, 0, "tape_pools.pool_id")?);
    }
    Ok(pool_ids)
}

struct ObjectCopyProjectionRow<'a> {
    object_id: &'a str,
    tape_uuid: [u8; 16],
    tape_file_number: u32,
    first_body_lba: u64,
    first_parity_data_ordinal: Option<u64>,
    protected_until_ordinal: Option<u64>,
    status: &'a str,
    representation: Option<&'a str>,
    key_id: Option<&'a [u8]>,
    metadata_frame_len: Option<u64>,
    plaintext_digest: Option<&'a [u8]>,
    stored_digest: Option<&'a [u8]>,
}

struct ObjectCopyEnvelopeProjection<'a> {
    representation: Option<&'static str>,
    key_id: Option<&'a [u8]>,
    metadata_frame_len: Option<u64>,
}

fn object_copy_envelope_from_tape_entry(
    entry: &TapeFileEntry,
) -> Result<ObjectCopyEnvelopeProjection<'_>, StateError> {
    let Some(row) = entry.bootstrap_object_row.as_ref() else {
        return Ok(ObjectCopyEnvelopeProjection {
            representation: Some(OBJECT_COPY_REPRESENTATION_UNKNOWN),
            key_id: None,
            metadata_frame_len: None,
        });
    };
    if row.tape_file_number != entry.tape_file_number {
        return Err(StateError::IndexMigrationFailed(format!(
            "bootstrap object row tape file {} does not match journal entry {}",
            row.tape_file_number, entry.tape_file_number
        )));
    }
    if row.stored_block_count != entry.block_count {
        return Err(StateError::IndexMigrationFailed(format!(
            "bootstrap object row block count {} does not match journal entry {}",
            row.stored_block_count, entry.block_count
        )));
    }
    match &row.representation {
        BootstrapObjectRepresentation::Plaintext { .. } => Ok(ObjectCopyEnvelopeProjection {
            representation: Some(OBJECT_COPY_REPRESENTATION_PLAINTEXT),
            key_id: None,
            metadata_frame_len: None,
        }),
        BootstrapObjectRepresentation::Encrypted {
            key_id,
            metadata_frame_len,
        } => Ok(ObjectCopyEnvelopeProjection {
            representation: Some(OBJECT_COPY_REPRESENTATION_ENCRYPTED),
            key_id: Some(key_id.as_slice()),
            metadata_frame_len: Some(*metadata_frame_len),
        }),
    }
}

fn insert_object_copy_projection_tx(
    tx: &rusqlite::Transaction<'_>,
    row: ObjectCopyProjectionRow<'_>,
) -> Result<(), StateError> {
    validate_object_copy_envelope(row.representation, row.key_id, row.metadata_frame_len)?;
    validate_optional_sha256(row.plaintext_digest, "object_copies.plaintext_digest")?;
    validate_optional_sha256(row.stored_digest, "object_copies.stored_digest")?;
    let metadata_frame_len =
        opt_u64_to_i64(row.metadata_frame_len, "object_copies.metadata_frame_len")?;
    tx.execute(
        "insert into object_copies(
           object_id, tape_uuid, tape_file_number,
           first_body_lba, first_parity_data_ordinal,
           protected_until_ordinal, status, representation, key_id,
           metadata_frame_len, plaintext_digest, stored_digest, pool_id
         )
         values(
           ?1, ?2, ?3, ?4, ?5, ?6, ?7, coalesce(?8, 'unknown'), ?9, ?10,
           ?11, ?12,
           (select pool_id from tapes where tape_uuid = ?2)
         )
         on conflict(object_id, tape_uuid, tape_file_number) do update set
           first_body_lba =
             case
               when excluded.first_body_lba != 0 then excluded.first_body_lba
               else object_copies.first_body_lba
             end,
           first_parity_data_ordinal = excluded.first_parity_data_ordinal,
           protected_until_ordinal = excluded.protected_until_ordinal,
           status = excluded.status,
           representation =
             case
               when ?8 is not null and ?8 != 'unknown' then excluded.representation
               else object_copies.representation
             end,
           key_id =
             case
               when ?8 is not null and ?8 != 'unknown' then excluded.key_id
               else object_copies.key_id
             end,
           metadata_frame_len =
             case
               when ?8 is not null and ?8 != 'unknown' then excluded.metadata_frame_len
               else object_copies.metadata_frame_len
             end,
           plaintext_digest = coalesce(excluded.plaintext_digest, object_copies.plaintext_digest),
           stored_digest = coalesce(excluded.stored_digest, object_copies.stored_digest),
           pool_id = coalesce(object_copies.pool_id, excluded.pool_id)",
        params![
            row.object_id,
            row.tape_uuid.to_vec(),
            i64::from(row.tape_file_number),
            u64_to_i64(row.first_body_lba, "first_body_lba")?,
            opt_u64_to_i64(row.first_parity_data_ordinal, "first_parity_data_ordinal")?,
            opt_u64_to_i64(row.protected_until_ordinal, "protected_until_ordinal")?,
            row.status,
            row.representation,
            row.key_id,
            metadata_frame_len,
            row.plaintext_digest,
            row.stored_digest,
        ],
    )
    .map_err(|err| sqlite_error("upsert object_copy projection", err))?;
    Ok(())
}

fn validate_optional_sha256(value: Option<&[u8]>, field: &str) -> Result<(), StateError> {
    if let Some(value) = value {
        if value.len() != 32 {
            return Err(StateError::IndexMigrationFailed(format!(
                "{field} must be exactly 32 bytes"
            )));
        }
    }
    Ok(())
}

fn validate_object_copy_envelope(
    representation: Option<&str>,
    key_id: Option<&[u8]>,
    metadata_frame_len: Option<u64>,
) -> Result<(), StateError> {
    let Some(representation) = representation else {
        if key_id.is_some() || metadata_frame_len.is_some() {
            return Err(StateError::IndexMigrationFailed(
                "object copy envelope details require an explicit representation".to_string(),
            ));
        }
        return Ok(());
    };
    match representation {
        OBJECT_COPY_REPRESENTATION_PLAINTEXT => {
            if key_id.is_some() || metadata_frame_len.is_some() {
                return Err(StateError::IndexMigrationFailed(
                    "plaintext object copy rows must not carry encrypted envelope fields"
                        .to_string(),
                ));
            }
            Ok(())
        }
        OBJECT_COPY_REPRESENTATION_UNKNOWN => {
            if key_id.is_some() || metadata_frame_len.is_some() {
                return Err(StateError::IndexMigrationFailed(
                    "unknown object copy rows must not carry encrypted envelope fields".to_string(),
                ));
            }
            Ok(())
        }
        OBJECT_COPY_REPRESENTATION_ENCRYPTED => {
            let Some(key_id) = key_id else {
                return Err(StateError::IndexMigrationFailed(
                    "encrypted object copy rows require key_id".to_string(),
                ));
            };
            if key_id.len() != 16 || key_id.iter().all(|byte| *byte == 0) {
                return Err(StateError::IndexMigrationFailed(
                    "encrypted object copy key_id must be 16 nonzero bytes".to_string(),
                ));
            }
            let Some(metadata_frame_len) = metadata_frame_len else {
                return Err(StateError::IndexMigrationFailed(
                    "encrypted object copy rows require metadata_frame_len".to_string(),
                ));
            };
            if !(17..=16 * 1024 * 1024).contains(&metadata_frame_len) {
                return Err(StateError::IndexMigrationFailed(
                    "encrypted object copy metadata_frame_len must be in [17, 16 MiB]".to_string(),
                ));
            }
            Ok(())
        }
        other => Err(StateError::IndexMigrationFailed(format!(
            "unsupported object copy representation {other}"
        ))),
    }
}

fn insert_native_catalog_unit_tx(
    tx: &rusqlite::Transaction<'_>,
    object_id: &str,
    tape_uuid: [u8; 16],
    tape_file_number: u32,
    format_id: &str,
    created_at_utc: &str,
) -> Result<(), StateError> {
    tx.execute(
        "insert into catalog_units(
           unit_id, tape_uuid, origin_kind, format_id, native_object_id,
           scan_id, confidence, entry_count, damage_event_count,
           last_scan_at_utc, adapter_state, created_at_utc
         )
         values(?1, ?2, 'native_object', ?3, ?4,
                null, null, null, null, null, ?5, ?6)
         on conflict(unit_id) do update set
           tape_uuid = excluded.tape_uuid,
           origin_kind = excluded.origin_kind,
           format_id = excluded.format_id,
           native_object_id = excluded.native_object_id",
        params![
            native_catalog_unit_id(object_id, tape_uuid, tape_file_number),
            tape_uuid.to_vec(),
            format_id,
            object_id,
            Vec::<u8>::new(),
            created_at_utc,
        ],
    )
    .map_err(|err| sqlite_error("upsert native catalog unit projection", err))?;
    Ok(())
}

fn native_object_format_id_tx(
    tx: &rusqlite::Transaction<'_>,
    object_id: &str,
) -> Result<String, StateError> {
    tx.query_row(
        "select body_format from objects where object_id = ?1",
        params![object_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .map_err(|err| sqlite_error("lookup native object format", err))
    .map(|value| value.flatten().unwrap_or_else(|| "unknown".to_string()))
}

fn native_catalog_unit_id(object_id: &str, tape_uuid: [u8; 16], tape_file_number: u32) -> String {
    format!(
        "native:{}:{tape_file_number}:{object_id}",
        hex_uuid(tape_uuid)
    )
}

fn foreign_catalog_unit_id(source_kind: &str, source_id: &str, scan_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_kind.as_bytes());
    hasher.update([0]);
    hasher.update(source_id.as_bytes());
    let digest = hasher.finalize();
    format!("foreign:{source_kind}:{}:{scan_id}", hex_bytes(&digest))
}

fn tape_file_kind(kind: TapeFileKind) -> &'static str {
    match kind {
        TapeFileKind::Object => "object",
        TapeFileKind::ParitySidecar => "parity_sidecar",
        TapeFileKind::ParityMap => "parity_map",
        TapeFileKind::Bootstrap => "bootstrap",
    }
}

fn u64_to_i64(value: u64, field: &str) -> Result<i64, StateError> {
    i64::try_from(value)
        .map_err(|_| StateError::IndexMigrationFailed(format!("{field} exceeds i64 range")))
}

fn opt_u64_to_i64(value: Option<u64>, field: &str) -> Result<Option<i64>, StateError> {
    value.map(|value| u64_to_i64(value, field)).transpose()
}

fn i64_to_u64(value: i64, field: &str) -> Result<u64, StateError> {
    u64::try_from(value)
        .map_err(|_| StateError::IndexCorrupt(format!("{field} is negative or exceeds u64 range")))
}

fn opt_i64_to_u64(value: Option<i64>, field: &str) -> Result<Option<u64>, StateError> {
    value.map(|value| i64_to_u64(value, field)).transpose()
}

fn i64_to_u32(value: i64, field: &str) -> Result<u32, StateError> {
    u32::try_from(value)
        .map_err(|_| StateError::IndexCorrupt(format!("{field} is outside u32 range")))
}

fn opt_i64_to_u32(value: Option<i64>, field: &str) -> Result<Option<u32>, StateError> {
    value.map(|value| i64_to_u32(value, field)).transpose()
}

fn row_get<T: rusqlite::types::FromSql>(
    row: &rusqlite::Row<'_>,
    idx: usize,
    field: &str,
) -> Result<T, StateError> {
    row.get(idx)
        .map_err(|err| sqlite_error(&format!("read {field}"), err))
}

fn operation_from_row(row: &rusqlite::Row<'_>) -> Result<OperationRecord, StateError> {
    Ok(OperationRecord {
        operation_id: row_get(row, 0, "operations.operation_id")?,
        operation_kind: row_get(row, 1, "operations.operation_kind")?,
        state: row_get(row, 2, "operations.state")?,
        session_id: row_get(row, 3, "operations.session_id")?,
        subject: row_get(row, 4, "operations.subject")?,
        started_at_utc: row_get(row, 5, "operations.started_at_utc")?,
        updated_at_utc: row_get(row, 6, "operations.updated_at_utc")?,
    })
}

fn tape_pool_from_row(row: &rusqlite::Row<'_>) -> Result<TapePoolRecord, StateError> {
    Ok(TapePoolRecord {
        pool_id: row_get(row, 0, "tape_pools.pool_id")?,
        display_name: row_get(row, 1, "tape_pools.display_name")?,
        copy_class: row_get(row, 2, "tape_pools.copy_class")?,
        content_class: row_get(row, 3, "tape_pools.content_class")?,
        created_at_utc: row_get(row, 4, "tape_pools.created_at_utc")?,
    })
}

fn catalog_unit_from_row(row: &rusqlite::Row<'_>) -> Result<CatalogUnitRecord, StateError> {
    Ok(CatalogUnitRecord {
        unit_id: row_get(row, 0, "catalog_units.unit_id")?,
        tape_uuid: row_get(row, 1, "catalog_units.tape_uuid")?,
        origin_kind: row_get(row, 2, "catalog_units.origin_kind")?,
        format_id: row_get(row, 3, "catalog_units.format_id")?,
        native_object_id: row_get(row, 4, "catalog_units.native_object_id")?,
        scan_id: row_get(row, 5, "catalog_units.scan_id")?,
        source_kind: row_get(row, 6, "catalog_units.source_kind")?,
        source_id: row_get(row, 7, "catalog_units.source_id")?,
        confidence: row_get(row, 8, "catalog_units.confidence")?,
        entry_count: opt_i64_to_u64(row_get(row, 9, "catalog_units.entry_count")?, "entry_count")?,
        damage_event_count: opt_i64_to_u64(
            row_get(row, 10, "catalog_units.damage_event_count")?,
            "damage_event_count",
        )?,
        last_scan_at_utc: row_get(row, 11, "catalog_units.last_scan_at_utc")?,
        adapter_state: row_get(row, 12, "catalog_units.adapter_state")?,
        created_at_utc: row_get(row, 13, "catalog_units.created_at_utc")?,
    })
}

fn native_object_from_row(row: &rusqlite::Row<'_>) -> Result<NativeObjectRecord, StateError> {
    Ok(NativeObjectRecord {
        object_id: row_get(row, 0, "objects.object_id")?,
        caller_object_id: row_get(row, 1, "objects.caller_object_id")?,
        body_format: row_get::<Option<String>>(row, 2, "objects.body_format")?
            .unwrap_or_else(|| "unknown".to_string()),
        logical_size_bytes: opt_i64_to_u64(
            row_get(row, 3, "objects.logical_size_bytes")?,
            "logical_size_bytes",
        )?,
        content_hash: row_get(row, 4, "objects.content_hash")?,
        metadata_hash: row_get(row, 5, "objects.metadata_hash")?,
        created_at_utc: row_get(row, 6, "objects.created_at_utc")?,
        copies: Vec::new(),
    })
}

fn native_object_copy_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<NativeObjectCopyRecord, StateError> {
    Ok(NativeObjectCopyRecord {
        object_id: row_get(row, 0, "object_copies.object_id")?,
        tape_uuid: row_get(row, 1, "object_copies.tape_uuid")?,
        tape_file_number: i64_to_u32(
            row_get(row, 2, "object_copies.tape_file_number")?,
            "tape_file_number",
        )?,
        first_body_lba: i64_to_u64(
            row_get(row, 3, "object_copies.first_body_lba")?,
            "first_body_lba",
        )?,
        first_parity_data_ordinal: opt_i64_to_u64(
            row_get(row, 4, "object_copies.first_parity_data_ordinal")?,
            "first_parity_data_ordinal",
        )?,
        protected_until_ordinal: opt_i64_to_u64(
            row_get(row, 5, "object_copies.protected_until_ordinal")?,
            "protected_until_ordinal",
        )?,
        status: row_get(row, 6, "object_copies.status")?,
        pool_id: row_get(row, 7, "object_copies.pool_id")?,
        representation: row_get(row, 8, "object_copies.representation")?,
        key_id: row_get(row, 9, "object_copies.key_id")?,
        metadata_frame_len: opt_i64_to_u64(
            row_get(row, 10, "object_copies.metadata_frame_len")?,
            "metadata_frame_len",
        )?,
        plaintext_digest: row_get(row, 11, "object_copies.plaintext_digest")?,
        stored_digest: row_get(row, 12, "object_copies.stored_digest")?,
    })
}

fn native_object_file_from_row(
    row: &rusqlite::Row<'_>,
) -> Result<NativeObjectFileRecord, StateError> {
    let executable = row_get::<Option<i64>>(row, 8, "object_files.executable")?
        .map(|value| match value {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(StateError::IndexCorrupt(
                "object_files.executable is not boolean".to_string(),
            )),
        })
        .transpose()?;
    Ok(NativeObjectFileRecord {
        object_id: row_get(row, 0, "object_files.object_id")?,
        file_id: row_get(row, 1, "object_files.file_id")?,
        path: row_get(row, 2, "object_files.path")?,
        size_bytes: i64_to_u64(row_get(row, 3, "object_files.size_bytes")?, "size_bytes")?,
        file_sha256: row_get(row, 4, "object_files.file_sha256")?,
        first_chunk_lba: opt_i64_to_u64(
            row_get(row, 5, "object_files.first_chunk_lba")?,
            "first_chunk_lba",
        )?,
        chunk_count: i64_to_u64(row_get(row, 6, "object_files.chunk_count")?, "chunk_count")?,
        mtime: row_get(row, 7, "object_files.mtime")?,
        executable,
    })
}

fn native_object_copy_from_join_row(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> Result<Option<NativeObjectCopyRecord>, StateError> {
    let object_id: Option<String> = row_get(row, offset, "object_copies.object_id")?;
    let Some(object_id) = object_id else {
        return Ok(None);
    };
    Ok(Some(NativeObjectCopyRecord {
        object_id,
        tape_uuid: row_get(row, offset + 1, "object_copies.tape_uuid")?,
        tape_file_number: i64_to_u32(
            row_get(row, offset + 2, "object_copies.tape_file_number")?,
            "tape_file_number",
        )?,
        first_body_lba: i64_to_u64(
            row_get(row, offset + 3, "object_copies.first_body_lba")?,
            "first_body_lba",
        )?,
        first_parity_data_ordinal: opt_i64_to_u64(
            row_get(row, offset + 4, "object_copies.first_parity_data_ordinal")?,
            "first_parity_data_ordinal",
        )?,
        protected_until_ordinal: opt_i64_to_u64(
            row_get(row, offset + 5, "object_copies.protected_until_ordinal")?,
            "protected_until_ordinal",
        )?,
        status: row_get(row, offset + 6, "object_copies.status")?,
        pool_id: row_get(row, offset + 7, "object_copies.pool_id")?,
        representation: row_get(row, offset + 8, "object_copies.representation")?,
        key_id: row_get(row, offset + 9, "object_copies.key_id")?,
        metadata_frame_len: opt_i64_to_u64(
            row_get(row, offset + 10, "object_copies.metadata_frame_len")?,
            "metadata_frame_len",
        )?,
        plaintext_digest: row_get(row, offset + 11, "object_copies.plaintext_digest")?,
        stored_digest: row_get(row, offset + 12, "object_copies.stored_digest")?,
    }))
}

fn tape_from_row(row: &rusqlite::Row<'_>) -> Result<TapeRecord, StateError> {
    Ok(TapeRecord {
        tape_uuid: row_get(row, 0, "tapes.tape_uuid")?,
        voltag: row_get(row, 1, "tapes.voltag")?,
        pool_id: row_get(row, 2, "tapes.pool_id")?,
        body_format: row_get(row, 3, "tapes.body_format")?,
        block_size: opt_i64_to_u64(row_get(row, 4, "tapes.block_size")?, "block_size")?,
        scheme_id: row_get(row, 5, "tapes.scheme_id")?,
        data_blocks_per_stripe: opt_i64_to_u32(
            row_get(row, 6, "tapes.data_blocks_per_stripe")?,
            "data_blocks_per_stripe",
        )?,
        parity_blocks_per_stripe: opt_i64_to_u32(
            row_get(row, 7, "tapes.parity_blocks_per_stripe")?,
            "parity_blocks_per_stripe",
        )?,
        stripes_per_neighborhood: opt_i64_to_u32(
            row_get(row, 8, "tapes.stripes_per_neighborhood")?,
            "stripes_per_neighborhood",
        )?,
        last_committed_tape_file: opt_i64_to_u64(
            row_get(row, 9, "tapes.last_committed_tape_file")?,
            "last_committed_tape_file",
        )?,
        total_committed_ordinals: i64_to_u64(
            row_get(row, 10, "tapes.total_committed_ordinals")?,
            "total_committed_ordinals",
        )?,
        state: row_get(row, 11, "tapes.state")?,
        updated_at_utc: row_get(row, 12, "tapes.updated_at_utc")?,
    })
}

fn tape_file_from_row(row: &rusqlite::Row<'_>) -> Result<TapeFileRecord, StateError> {
    Ok(TapeFileRecord {
        tape_uuid: row_get(row, 0, "tape_files.tape_uuid")?,
        tape_file_number: i64_to_u32(
            row_get(row, 1, "tape_files.tape_file_number")?,
            "tape_file_number",
        )?,
        kind: row_get(row, 2, "tape_files.kind")?,
        block_count: i64_to_u64(row_get(row, 3, "tape_files.block_count")?, "block_count")?,
        object_id: row_get(row, 4, "tape_files.object_id")?,
    })
}

fn now_utc() -> Result<String, StateError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|err| StateError::IndexMigrationFailed(format!("format utc timestamp: {err}")))
}

fn normalize_pool_id(value: &str) -> Result<String, StateError> {
    let pool_id = value.trim();
    if pool_id.is_empty() {
        return Err(StateError::ConfigInvalid(
            "tape pool id must not be empty".to_string(),
        ));
    }
    if !pool_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':'))
    {
        return Err(StateError::ConfigInvalid(format!(
            "tape pool id {pool_id:?} must use only ASCII letters, digits, '.', '_', '-', or ':'"
        )));
    }
    Ok(pool_id.to_string())
}

fn hex_uuid(tape_uuid: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in tape_uuid {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("write to string");
    }
    out
}

fn hex_uuid_from_slice(tape_uuid: &[u8]) -> String {
    let mut out = String::with_capacity(tape_uuid.len() * 2);
    for byte in tape_uuid {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("write to string");
    }
    out
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("write to string");
    }
    out
}

fn configure_sqlite(conn: &Connection) -> Result<(), StateError> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|err| sqlite_error("set sqlite journal_mode", err))?;
    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(|err| sqlite_error("set sqlite busy_timeout", err))?;
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|err| sqlite_error("set sqlite synchronous", err))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|err| sqlite_error("set sqlite foreign_keys", err))?;
    Ok(())
}

fn configure_read_only_sqlite(conn: &Connection) -> Result<(), StateError> {
    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(|err| sqlite_error("set sqlite busy_timeout", err))?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|err| sqlite_error("set sqlite foreign_keys", err))?;
    Ok(())
}

fn validate_schema(conn: &Connection) -> Result<(), StateError> {
    let current = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .map_err(|err| sqlite_error("read sqlite user_version", err))?;
    if current == SCHEMA_VERSION {
        return Ok(());
    }
    if current > SCHEMA_VERSION {
        return Err(StateError::IndexMigrationFailed(format!(
            "sqlite user_version {current} is newer than supported {SCHEMA_VERSION}"
        )));
    }
    Err(StateError::IndexMigrationFailed(format!(
        "sqlite user_version {current} is older than supported {SCHEMA_VERSION}; open read-write to migrate"
    )))
}

fn migrate(conn: &Connection) -> Result<(), StateError> {
    let current = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
        .map_err(|err| sqlite_error("read sqlite user_version", err))?;
    if current > SCHEMA_VERSION {
        return Err(StateError::IndexMigrationFailed(format!(
            "sqlite user_version {current} is newer than supported {SCHEMA_VERSION}"
        )));
    }

    conn.execute_batch(MINIMUM_SCHEMA)
        .map_err(|err| sqlite_error("apply sqlite schema", err))?;
    ensure_column(
        conn,
        "object_copies",
        "first_body_lba",
        "first_body_lba integer not null default 0",
    )?;
    ensure_column(conn, "object_copies", "pool_id", "pool_id text")?;
    ensure_column(
        conn,
        "object_copies",
        "representation",
        "representation text not null default 'unknown'",
    )?;
    ensure_column(conn, "object_copies", "key_id", "key_id blob")?;
    ensure_column(
        conn,
        "object_copies",
        "metadata_frame_len",
        "metadata_frame_len integer",
    )?;
    ensure_column(
        conn,
        "object_copies",
        "plaintext_digest",
        "plaintext_digest blob",
    )?;
    ensure_column(conn, "object_copies", "stored_digest", "stored_digest blob")?;
    ensure_column(conn, "catalog_units", "source_kind", "source_kind text")?;
    ensure_column(conn, "catalog_units", "source_id", "source_id text")?;
    ensure_column(conn, "tapes", "pool_id", "pool_id text")?;
    if table_exists(conn, LEGACY_TAPE_POOL_MEMBERSHIPS_TABLE)? {
        let backfill_sql = format!(
            "update tapes set pool_id = (
                 select m.pool_id from {LEGACY_TAPE_POOL_MEMBERSHIPS_TABLE} m
                 where m.tape_uuid = tapes.tape_uuid
             )
             where pool_id is null
               and exists (
                 select 1 from {LEGACY_TAPE_POOL_MEMBERSHIPS_TABLE} m
                 where m.tape_uuid = tapes.tape_uuid
               )"
        );
        conn.execute(&backfill_sql, [])
            .map_err(|err| sqlite_error("backfill tapes.pool_id", err))?;
        let drop_sql = format!("drop table {LEGACY_TAPE_POOL_MEMBERSHIPS_TABLE}");
        conn.execute(&drop_sql, [])
            .map_err(|err| sqlite_error("drop legacy tape pool membership table", err))?;
    }
    conn.execute(
        "create index if not exists object_copies_pool_idx
         on object_copies(pool_id)
         where pool_id is not null",
        [],
    )
    .map_err(|err| sqlite_error("create object_copies_pool_idx", err))?;
    conn.execute(
        "create unique index if not exists tapes_voltag_unique
         on tapes(voltag)
         where voltag is not null",
        [],
    )
    .map_err(|err| sqlite_error("create tapes_voltag_unique", err))?;
    conn.execute(
        "create index if not exists tapes_pool_idx
         on tapes(pool_id)
         where pool_id is not null",
        [],
    )
    .map_err(|err| sqlite_error("create tapes_pool_idx", err))?;
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(|err| sqlite_error("set sqlite user_version", err))?;
    conn.execute(
        "insert into schema_meta(key, value)
         values('schema_version', ?1)
         on conflict(key) do update set value = excluded.value",
        params![SCHEMA_VERSION.to_string().into_bytes()],
    )
    .map_err(|err| sqlite_error("write schema_meta schema_version", err))?;
    Ok(())
}

fn ensure_column(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
    column_ddl: &str,
) -> Result<(), StateError> {
    if table_column_exists(conn, table_name, column_name)? {
        return Ok(());
    }
    let sql = format!("alter table {table_name} add column {column_ddl}");
    conn.execute(&sql, [])
        .map(|_| ())
        .map_err(|err| sqlite_error(&format!("add {table_name}.{column_name} column"), err))
}

fn table_exists(conn: &Connection, table_name: &str) -> Result<bool, StateError> {
    let found: Option<String> = conn
        .query_row(
            "select name from sqlite_master where type = 'table' and name = ?1",
            params![table_name],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| sqlite_error("check sqlite table existence", err))?;
    Ok(found.is_some())
}

fn table_column_exists(
    conn: &Connection,
    table_name: &str,
    column_name: &str,
) -> Result<bool, StateError> {
    let sql = format!("PRAGMA table_info({table_name})");
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|err| sqlite_error("prepare sqlite table_info", err))?;
    let mut rows = stmt
        .query([])
        .map_err(|err| sqlite_error("query sqlite table_info", err))?;
    while let Some(row) = rows
        .next()
        .map_err(|err| sqlite_error("iterate sqlite table_info", err))?
    {
        let name: String = row_get(row, 1, "pragma_table_info.name")?;
        if name == column_name {
            return Ok(true);
        }
    }
    Ok(false)
}

const MINIMUM_SCHEMA: &str = r#"
create table if not exists schema_meta(
  key text primary key,
  value blob not null
);

create table if not exists ingested_sources(
  source_kind text not null,
  source_id text not null,
  offset_bytes integer not null,
  terminal_hash blob,
  updated_at_utc text not null,
  primary key(source_kind, source_id)
);

create table if not exists tapes(
  tape_uuid blob primary key,
  voltag text,
  pool_id text,
  block_size integer,
  scheme_id text,
  data_blocks_per_stripe integer,
  parity_blocks_per_stripe integer,
  stripes_per_neighborhood integer,
  highest_protected_ordinal integer not null default 0,
  total_committed_ordinals integer not null default 0,
  last_committed_tape_file integer,
  state text not null,
  updated_at_utc text not null
);

create unique index if not exists tapes_voltag_unique
  on tapes(voltag)
  where voltag is not null;

create table if not exists tape_pools(
  pool_id text primary key,
  display_name text,
  copy_class text,
  content_class text,
  created_at_utc text not null
);

create table if not exists tape_files(
  tape_uuid blob not null,
  tape_file_number integer not null,
  kind text not null,
  block_count integer not null,
  physical_start_hint integer,
  object_id text,
  first_parity_data_ordinal integer,
  epoch_id integer,
  protected_ordinal_start integer,
  protected_ordinal_end_exclusive integer,
  canonical_metadata_hash blob,
  bundle_uuid text,
  bundle_kind text,
  primary key(tape_uuid, tape_file_number)
);

create table if not exists objects(
  object_id text primary key,
  caller_object_id text,
  body_format text,
  logical_size_bytes integer,
  content_hash blob,
  metadata_hash blob,
  created_at_utc text not null
);

create index if not exists objects_content_hash_idx
  on objects(content_hash)
  where content_hash is not null;

create index if not exists objects_caller_object_id_idx
  on objects(caller_object_id)
  where caller_object_id is not null;

create table if not exists object_copies(
  object_id text not null,
  tape_uuid blob not null,
  tape_file_number integer not null,
  first_body_lba integer not null default 0,
  first_parity_data_ordinal integer,
  protected_until_ordinal integer,
  status text not null,
  representation text not null default 'unknown',
  key_id blob,
  metadata_frame_len integer,
  plaintext_digest blob,
  stored_digest blob,
  pool_id text,
  primary key(object_id, tape_uuid, tape_file_number)
);

create table if not exists object_files(
  object_id text not null,
  file_id text not null,
  path text not null,
  size_bytes integer not null,
  file_sha256 blob not null,
  first_chunk_lba integer,
  chunk_count integer not null,
  mtime text,
  executable integer,
  primary key(object_id, file_id)
);

create index if not exists object_files_path_idx
  on object_files(object_id, path);

create table if not exists catalog_units(
  unit_id text primary key,
  tape_uuid blob not null,
  origin_kind text not null,
  format_id text not null,
  native_object_id text,
  scan_id text,
  source_kind text,
  source_id text,
  confidence text,
  entry_count integer,
  damage_event_count integer,
  last_scan_at_utc text,
  adapter_state blob not null default X'',
  created_at_utc text not null
);

create index if not exists catalog_units_tape_origin_idx
  on catalog_units(tape_uuid, origin_kind);

create index if not exists catalog_units_native_object_idx
  on catalog_units(native_object_id);

create table if not exists idempotency_keys(
  actor_fingerprint text not null,
  idempotency_key text not null,
  request_fingerprint blob not null,
  operation_id text not null,
  terminal_state text,
  response_fingerprint blob,
  updated_at_utc text not null,
  primary key(actor_fingerprint, idempotency_key)
);

create table if not exists operations(
  operation_id text primary key,
  operation_kind text not null,
  state text not null,
  session_id text,
  subject text,
  started_at_utc text not null,
  updated_at_utc text not null
);

create table if not exists sessions(
  session_id text primary key,
  session_kind text not null,
  tape_uuid blob,
  library_serial text,
  drive_bay integer,
  state text not null,
  opened_at_utc text not null,
  updated_at_utc text not null
);
"#;

fn sqlite_open_error(path: &Path, err: rusqlite::Error) -> StateError {
    StateError::Index {
        context: format!("open sqlite {}", path.display()),
        source: err,
    }
}

fn sqlite_error(context: &str, err: rusqlite::Error) -> StateError {
    StateError::Index {
        context: context.to_string(),
        source: err,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use ciborium::value::Value as CborValue;
    use remanence_parity::{
        BootstrapObjectRow, CommittedBundleKind, CommittedState, SchemeId, TapeFileEntry,
    };

    use super::*;

    const MINIMUM_TABLES: &[&str] = &[
        "schema_meta",
        "ingested_sources",
        "tapes",
        "tape_pools",
        "tape_files",
        "objects",
        "object_copies",
        "catalog_units",
        "idempotency_keys",
        "operations",
        "sessions",
    ];

    fn audit_record(
        sequence: u64,
        event: AuditEvent,
        operation_id: Option<Uuid>,
        session_id: Option<Uuid>,
        idempotency_key: Option<Uuid>,
        subject_kind: &str,
        detail: BTreeMap<String, CborValue>,
    ) -> AuditRecord {
        AuditRecord {
            schema_version: 1,
            record_uuid: Uuid::from_u128(sequence as u128),
            sequence,
            timestamp_utc: format!("2026-05-27T10:{sequence:02}:00Z"),
            host_id: "host".to_string(),
            process_id: 123,
            actor: AuditActor::User("alice".to_string()),
            source_layer: crate::audit::SourceLayer::Layer5,
            operation_id,
            session_id,
            idempotency_key,
            event,
            subject: crate::audit::AuditSubject {
                kind: subject_kind.to_string(),
                id: Some("subject-1".to_string()),
            },
            detail,
        }
    }

    fn detail(entries: &[(&str, CborValue)]) -> BTreeMap<String, CborValue> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), value.clone()))
            .collect()
    }

    fn rebuild_fixture() -> (TapeJournalIndexInput, CommittedState) {
        let scheme = ParityScheme {
            id: SchemeId::new_static("test-scheme"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 3,
        };
        let state = CommittedState {
            entries: vec![
                TapeFileEntry {
                    tape_file_number: 1,
                    kind: TapeFileKind::Object,
                    block_count: 3,
                    physical_start_hint: Some(10),
                    object_id: Some("object-1".to_string()),
                    first_parity_data_ordinal: Some(0),
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    bootstrap_object_row: None,
                },
                TapeFileEntry {
                    tape_file_number: 2,
                    kind: TapeFileKind::ParitySidecar,
                    block_count: 2,
                    physical_start_hint: Some(13),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: Some(0),
                    protected_ordinal_start: Some(0),
                    protected_ordinal_end_exclusive: Some(3),
                    canonical_metadata_hash: Some([9u8; 32]),
                    bootstrap_object_row: None,
                },
                TapeFileEntry {
                    tape_file_number: 3,
                    kind: TapeFileKind::ParityMap,
                    block_count: 1,
                    physical_start_hint: Some(15),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: Some(0),
                    protected_ordinal_start: Some(0),
                    protected_ordinal_end_exclusive: Some(3),
                    canonical_metadata_hash: Some([8u8; 32]),
                    bootstrap_object_row: None,
                },
                TapeFileEntry {
                    tape_file_number: 4,
                    kind: TapeFileKind::Bootstrap,
                    block_count: 1,
                    physical_start_hint: Some(16),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: Some([7u8; 32]),
                    bootstrap_object_row: None,
                },
            ],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        };
        (
            TapeJournalIndexInput {
                tape_uuid: [7u8; 16],
                block_size: 4096,
                scheme: Some(scheme),
                journal_offset_bytes: 123,
            },
            state,
        )
    }

    fn catalog_snapshot(index: &CatalogIndex) -> Vec<String> {
        let mut rows = Vec::new();
        rows.extend(query_strings(
            index,
            "select 'tape|' || hex(tape_uuid) || '|' || state || '|' ||
                    coalesce(last_committed_tape_file, '')
             from tapes order by hex(tape_uuid)",
        ));
        rows.extend(query_strings(
            index,
            "select 'file|' || hex(tape_uuid) || '|' || tape_file_number || '|' ||
                    kind || '|' || block_count || '|' || coalesce(object_id, '')
             from tape_files order by hex(tape_uuid), tape_file_number",
        ));
        rows.extend(query_strings(
            index,
            "select 'copy|' || object_id || '|' || hex(tape_uuid) || '|' ||
                    tape_file_number || '|' || status || '|' || coalesce(pool_id, '')
             from object_copies order by object_id, hex(tape_uuid), tape_file_number",
        ));
        rows.extend(query_strings(
            index,
            "select 'unit|' || unit_id || '|' || hex(tape_uuid) || '|' ||
                    origin_kind || '|' || format_id || '|' ||
                    coalesce(native_object_id, '')
             from catalog_units order by unit_id",
        ));
        rows
    }

    fn query_strings(index: &CatalogIndex, sql: &str) -> Vec<String> {
        let mut stmt = index.conn.prepare(sql).expect("prepare snapshot query");
        stmt.query_map([], |row| row.get::<_, String>(0))
            .expect("query snapshot rows")
            .map(|row| row.expect("snapshot row"))
            .collect()
    }

    fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    }

    fn provision_scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("rs-provision-test"),
            data_blocks_per_stripe: 8,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 16,
        }
    }

    fn pool_projection(pool_id: &str) -> TapePoolProjectionInput {
        TapePoolProjectionInput {
            pool_id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        }
    }

    fn pool_rule(prefix: &str, pool_id: &str) -> TapePoolRuleConfig {
        TapePoolRuleConfig {
            prefix: prefix.to_string(),
            pool_id: pool_id.to_string(),
        }
    }

    fn written_bootstrap_state() -> CommittedState {
        CommittedState {
            entries: vec![TapeFileEntry {
                tape_file_number: 0,
                kind: TapeFileKind::Bootstrap,
                block_count: 1,
                physical_start_hint: Some(0),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: Some([1u8; 32]),
                bootstrap_object_row: None,
            }],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        }
    }

    fn count_rows_for_tape(index: &CatalogIndex, table: &str, tape_uuid: [u8; 16]) -> u64 {
        let sql = format!("select count(*) from {table} where tape_uuid = ?1");
        index
            .conn
            .query_row(&sql, params![tape_uuid.to_vec()], |row| {
                row.get::<_, u64>(0)
            })
            .expect("count tape rows")
    }

    fn highest_protected_ordinal(index: &CatalogIndex, tape_uuid: [u8; 16]) -> u64 {
        index
            .conn
            .query_row(
                "select highest_protected_ordinal from tapes where tape_uuid = ?1",
                params![tape_uuid.to_vec()],
                |row| row.get::<_, u64>(0),
            )
            .expect("highest protected ordinal")
    }

    #[test]
    fn provision_tape_writes_ready_no_parity_row_without_membership() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [31u8; 16];

        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN001L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision no-parity tape");

        let tape = index
            .get_tape(&tape_uuid)
            .expect("get tape")
            .expect("tape row");
        assert_eq!(tape.voltag.as_deref(), Some("RMN001L9"));
        assert_eq!(tape.block_size, Some(4096));
        assert_eq!(tape.scheme_id, None);
        assert_eq!(tape.data_blocks_per_stripe, None);
        assert_eq!(tape.parity_blocks_per_stripe, None);
        assert_eq!(tape.stripes_per_neighborhood, None);
        assert_eq!(tape.last_committed_tape_file, None);
        assert_eq!(tape.total_committed_ordinals, 0);
        assert_eq!(tape.state, "ready");
        assert_eq!(tape.pool_id, None);
        assert_eq!(
            index
                .conn
                .query_row("select count(*) from object_copies", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("copy count"),
            0
        );
    }

    #[test]
    fn rules_reconcile_writes_and_clears_tapes_pool_id() {
        let dir = tempfile::Builder::new()
            .prefix("rem-pool-recon")
            .tempdir()
            .expect("tempdir");
        let mut index = CatalogIndex::open(dir.path().join("s.sqlite")).expect("open");
        let uuid = [7u8; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: uuid,
                voltag: "RMN001L9".to_string(),
                block_size: 65536,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision");

        let pools = vec![TapePoolProjectionInput {
            pool_id: "scenario-a".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        }];
        let rules = vec![TapePoolRuleConfig {
            prefix: "RMN".to_string(),
            pool_id: "scenario-a".to_string(),
        }];
        index
            .reconcile_tape_pool_projection_from_rules(&pools, &rules)
            .expect("reconcile with rule");
        assert_eq!(
            index.get_tape_pool_membership(&uuid).expect("lookup"),
            Some("scenario-a".to_string())
        );

        index
            .reconcile_tape_pool_projection_from_rules(&pools, &[])
            .expect("reconcile no rules");
        assert_eq!(index.get_tape_pool_membership(&uuid).expect("lookup"), None);
    }

    #[test]
    fn list_tapes_filters_by_pool_id_column() {
        let dir = tempfile::Builder::new()
            .prefix("rem-pool-list")
            .tempdir()
            .expect("tempdir");
        let mut index = CatalogIndex::open(dir.path().join("s.sqlite")).expect("open");
        let uuid = [9u8; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: uuid,
                voltag: "RMN042L9".to_string(),
                block_size: 65536,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision");
        index
            .project_tape_pool_membership(uuid, "scenario-a")
            .expect("assign");

        let in_pool = index.list_tapes(Some("scenario-a")).expect("list in pool");
        assert_eq!(in_pool.len(), 1);
        assert_eq!(in_pool[0].pool_id.as_deref(), Some("scenario-a"));

        let other = index.list_tapes(Some("nope")).expect("list other pool");
        assert!(other.is_empty());
    }

    #[test]
    fn reconcile_tape_pool_projection_derives_memberships_from_voltags() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let default_uuid = [41u8; 16];
        let specific_uuid = [42u8; 16];
        let unmatched_uuid = [43u8; 16];

        for (tape_uuid, voltag) in [
            (default_uuid, "ACX001L9"),
            (specific_uuid, "ACM001L9"),
            (unmatched_uuid, "BCM001L9"),
        ] {
            index
                .provision_tape(ProvisionTapeInput {
                    tape_uuid,
                    voltag: voltag.to_string(),
                    block_size: 4096,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision tape");
        }

        index
            .reconcile_tape_pool_projection_from_rules(
                &[
                    pool_projection("camera.default"),
                    pool_projection("camera.copy-a"),
                ],
                &[
                    pool_rule("AC", "camera.default"),
                    pool_rule("ACM", "camera.copy-a"),
                ],
            )
            .expect("reconcile derived pools");

        let default_tape = index
            .get_tape(&default_uuid)
            .expect("get default tape")
            .expect("default tape");
        let specific_tape = index
            .get_tape(&specific_uuid)
            .expect("get specific tape")
            .expect("specific tape");
        let unmatched_tape = index
            .get_tape(&unmatched_uuid)
            .expect("get unmatched tape")
            .expect("unmatched tape");
        assert_eq!(default_tape.pool_id.as_deref(), Some("camera.default"));
        assert_eq!(specific_tape.pool_id.as_deref(), Some("camera.copy-a"));
        assert_eq!(unmatched_tape.pool_id, None);
    }

    #[test]
    fn derived_reconcile_preserves_committed_foreign_pool_safety() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [44u8; 16];
        let scheme = provision_scheme();

        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "ACM001L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision tape");
        index
            .reconcile_tape_pool_projection_from_rules(
                &[pool_projection("camera.copy-a")],
                &[pool_rule("ACM", "camera.copy-a")],
            )
            .expect("derive initial pool");
        index
            .project_native_object_and_committed_tape_file_bundle(
                NativeObjectProjectionInput {
                    object_id: "object-foreign-pool".to_string(),
                    caller_object_id: Some("caller-foreign-pool".to_string()),
                    body_format: "rao-v1".to_string(),
                    logical_size_bytes: Some(42),
                    content_hash: Some(vec![1u8; 32]),
                    metadata_hash: Some(vec![2u8; 32]),
                    created_at_utc: Some("2026-05-30T10:00:00Z".to_string()),
                },
                &[],
                &[NativeObjectCopyProjectionInput {
                    object_id: "object-foreign-pool".to_string(),
                    tape_uuid,
                    tape_file_number: 1,
                    first_body_lba: 0,
                    first_parity_data_ordinal: Some(0),
                    protected_until_ordinal: Some(3),
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                    key_id: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x31; 32]),
                    stored_digest: Some(vec![0x31; 32]),
                }],
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: 4096,
                    scheme: Some(scheme),
                    journal_offset_bytes: 0,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 1,
                        kind: TapeFileKind::Object,
                        block_count: 3,
                        physical_start_hint: Some(0),
                        object_id: Some("object-foreign-pool".to_string()),
                        first_parity_data_ordinal: Some(0),
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: None,
                    }],
                    highest_protected_ordinal: 3,
                    total_committed_ordinals: 3,
                },
            )
            .expect("project committed object");

        let err = index
            .reconcile_tape_pool_projection_from_rules(
                &[pool_projection("camera.copy-b")],
                &[pool_rule("ACM", "camera.copy-b")],
            )
            .expect_err("committed pool conflict must fail");
        assert!(
            matches!(err, StateError::TapePoolAssignmentConflict(_)),
            "{err}"
        );

        let tape = index
            .get_tape(&tape_uuid)
            .expect("get tape after failed reconcile")
            .expect("tape exists");
        assert_eq!(tape.pool_id.as_deref(), Some("camera.copy-a"));
    }

    #[test]
    fn provision_tape_writes_ready_parity_row() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [32u8; 16];
        let scheme = provision_scheme();

        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN002L9".to_string(),
                block_size: 262_144,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision parity tape");

        let tape = index
            .get_tape(&tape_uuid)
            .expect("get tape")
            .expect("tape row");
        assert_eq!(tape.voltag.as_deref(), Some("RMN002L9"));
        assert_eq!(tape.block_size, Some(262_144));
        assert_eq!(tape.scheme_id.as_deref(), Some(scheme.id.as_str()));
        assert_eq!(
            tape.data_blocks_per_stripe,
            Some(u32::from(scheme.data_blocks_per_stripe))
        );
        assert_eq!(
            tape.parity_blocks_per_stripe,
            Some(u32::from(scheme.parity_blocks_per_stripe))
        );
        assert_eq!(
            tape.stripes_per_neighborhood,
            Some(scheme.stripes_per_neighborhood)
        );
        assert_eq!(tape.state, "ready");
    }

    #[test]
    fn provision_tape_is_idempotent_for_identical_uuid_and_geometry() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [33u8; 16];
        let input = ProvisionTapeInput {
            tape_uuid,
            voltag: "RMN003L8".to_string(),
            block_size: 4096,
            parity: ParityConfig::None,
            force: false,
        };

        index
            .provision_tape(input.clone())
            .expect("first provision");
        let first = index.get_tape(&tape_uuid).expect("get first tape");
        index.provision_tape(input).expect("second provision");
        let second = index.get_tape(&tape_uuid).expect("get second tape");

        assert_eq!(second, first);
    }

    #[test]
    fn provision_tape_allows_geometry_change_for_unwritten_tape() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [34u8; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN004L8".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("first provision");

        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN004L8".to_string(),
                block_size: 8192,
                parity: ParityConfig::Scheme(provision_scheme()),
                force: false,
            })
            .expect("re-provision unwritten");

        let tape = index
            .get_tape(&tape_uuid)
            .expect("get tape")
            .expect("tape row");
        assert_eq!(tape.block_size, Some(8192));
        assert_eq!(tape.scheme_id.as_deref(), Some("rs-provision-test"));
        assert_eq!(tape.last_committed_tape_file, None);
        assert_eq!(tape.state, "ready");
    }

    #[test]
    fn provision_tape_refuses_geometry_change_for_written_tape_without_force() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [35u8; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN005L8".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .index_committed_tape_journal(
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: 4096,
                    scheme: None,
                    journal_offset_bytes: 0,
                },
                &written_bootstrap_state(),
            )
            .expect("mark tape written");

        let err = index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN005L8".to_string(),
                block_size: 8192,
                parity: ParityConfig::None,
                force: false,
            })
            .expect_err("written geometry change must fail");

        assert!(matches!(err, StateError::TapeProvisionConflict(_)), "{err}");
        let tape = index
            .get_tape(&tape_uuid)
            .expect("get tape")
            .expect("tape row");
        assert_eq!(tape.block_size, Some(4096));
        assert_eq!(tape.last_committed_tape_file, Some(0));
        assert_eq!(tape.state, "ingested");
    }

    #[test]
    fn provision_tape_corrects_voltag_without_resetting_written_state() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, state) = rebuild_fixture();
        let tape_uuid = input.tape_uuid;
        let scheme = input.scheme.clone().expect("scheme");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN006L8".to_string(),
                block_size: input.block_size,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision tape");
        index
            .index_committed_tape_journal(input, &state)
            .expect("mark tape written");

        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN006L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::Scheme(scheme),
                force: false,
            })
            .expect("correct voltag");

        let tape = index
            .get_tape(&tape_uuid)
            .expect("get tape")
            .expect("tape row");
        assert_eq!(tape.voltag.as_deref(), Some("RMN006L9"));
        assert_eq!(tape.state, "ingested");
        assert_eq!(tape.last_committed_tape_file, Some(4));
        assert_eq!(tape.total_committed_ordinals, 3);
        assert_eq!(highest_protected_ordinal(&index, tape_uuid), 3);
        assert_eq!(count_rows_for_tape(&index, "tape_files", tape_uuid), 4);
        assert_eq!(count_rows_for_tape(&index, "object_copies", tape_uuid), 1);
    }

    #[test]
    fn provision_tape_force_reprovision_of_written_tape_resets_projection_state() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, state) = rebuild_fixture();
        let old_tape_uuid = input.tape_uuid;
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: old_tape_uuid,
                voltag: "RMN007L8".to_string(),
                block_size: input.block_size,
                parity: ParityConfig::Scheme(input.scheme.clone().expect("scheme")),
                force: false,
            })
            .expect("provision tape");
        index
            .index_committed_tape_journal(input, &state)
            .expect("mark tape written");
        assert_eq!(count_rows_for_tape(&index, "tape_files", old_tape_uuid), 4);
        assert_eq!(
            count_rows_for_tape(&index, "object_copies", old_tape_uuid),
            1
        );

        let new_tape_uuid = [36u8; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: new_tape_uuid,
                voltag: "RMN007L8".to_string(),
                block_size: 8192,
                parity: ParityConfig::None,
                force: true,
            })
            .expect("force re-provision");

        assert!(index
            .get_tape(&old_tape_uuid)
            .expect("get old tape")
            .is_none());
        let tape = index
            .get_tape(&new_tape_uuid)
            .expect("get new tape")
            .expect("new tape row");
        assert_eq!(tape.voltag.as_deref(), Some("RMN007L8"));
        assert_eq!(tape.block_size, Some(8192));
        assert_eq!(tape.scheme_id, None);
        assert_eq!(tape.state, "ready");
        assert_eq!(tape.last_committed_tape_file, None);
        assert_eq!(tape.total_committed_ordinals, 0);
        assert_eq!(highest_protected_ordinal(&index, new_tape_uuid), 0);
        assert_eq!(count_rows_for_tape(&index, "tape_files", old_tape_uuid), 0);
        assert_eq!(
            count_rows_for_tape(&index, "object_copies", old_tape_uuid),
            0
        );
    }

    #[test]
    fn provision_tape_same_voltag_different_uuid_updates_existing_unwritten_row() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let old_tape_uuid = [37u8; 16];
        let new_tape_uuid = [38u8; 16];
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: old_tape_uuid,
                voltag: "RMN008L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision first tape");

        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: new_tape_uuid,
                voltag: "RMN008L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("re-provision same voltag");

        assert!(index
            .get_tape(&old_tape_uuid)
            .expect("get old tape")
            .is_none());
        assert!(index
            .get_tape(&new_tape_uuid)
            .expect("get new tape")
            .is_some());
        assert_eq!(
            index
                .conn
                .query_row(
                    "select count(*) from tapes where voltag = 'RMN008L9'",
                    [],
                    |row| { row.get::<_, u64>(0) }
                )
                .expect("voltag row count"),
            1
        );
    }

    #[test]
    fn migrations_create_minimum_tables_and_pragmas() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let index = CatalogIndex::open(temp.path().join("index/rem-state.sqlite")).expect("open");

        assert_eq!(
            index.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        assert_eq!(index.quick_check().expect("quick check"), "ok");
        for table in MINIMUM_TABLES {
            assert!(index.table_exists(table).expect("table exists"), "{table}");
        }
        assert_eq!(
            index
                .conn
                .query_row(
                    "select count(*) from sqlite_master
                     where type = 'index' and name = 'tapes_voltag_unique'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .expect("tapes voltag unique index"),
            1
        );
        assert_eq!(
            index
                .conn
                .query_row(
                    "select count(*) from sqlite_master
                     where type = 'index' and name = 'tapes_pool_idx'",
                    [],
                    |row| row.get::<_, u64>(0),
                )
                .expect("tapes pool index"),
            1
        );
        assert_eq!(
            index
                .conn
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .expect("journal_mode")
                .to_ascii_lowercase(),
            "wal"
        );
        assert_eq!(
            index
                .conn
                .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, u32>(0))
                .expect("foreign_keys"),
            1
        );
        assert_eq!(
            index
                .conn
                .query_row("PRAGMA synchronous", [], |row| row.get::<_, u32>(0))
                .expect("synchronous"),
            2
        );
    }

    #[test]
    fn migrated_tables_have_pool_and_copy_envelope_columns() {
        let dir = tempfile::Builder::new()
            .prefix("rem-pool-col")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("rem-state.sqlite");
        let index = CatalogIndex::open(&path).expect("open index");
        assert_eq!(
            index.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );

        let conn = Connection::open(&path).expect("open raw sqlite");
        assert!(
            table_column_exists(&conn, "tapes", "pool_id").expect("table_info"),
            "tapes.pool_id column must exist after migration"
        );
        for column in [
            "representation",
            "key_id",
            "metadata_frame_len",
            "plaintext_digest",
            "stored_digest",
        ] {
            assert!(
                table_column_exists(&conn, "object_copies", column).expect("table_info"),
                "object_copies.{column} column must exist after migration"
            );
        }
    }

    #[test]
    fn legacy_pool_membership_table_is_dropped() {
        let dir = tempfile::Builder::new()
            .prefix("rem-drop-tbl")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("s.sqlite");
        let _index = CatalogIndex::open(&path).expect("open");
        let conn = Connection::open(&path).expect("open raw sqlite");
        let exists: Option<String> = conn
            .query_row(
                "select name from sqlite_master where type='table' and name=?1",
                params![LEGACY_TAPE_POOL_MEMBERSHIPS_TABLE],
                |row| row.get(0),
            )
            .optional()
            .expect("query sqlite_master");
        assert!(
            exists.is_none(),
            "legacy pool membership table must be dropped"
        );
    }

    #[test]
    fn migrations_are_idempotent_and_preserve_rows() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let path = temp.path().join("rem-state.sqlite");
        let first = CatalogIndex::open(&path).expect("open");
        first
            .conn
            .execute(
                "insert into schema_meta(key, value) values('custom', ?1)",
                params![b"kept".as_slice()],
            )
            .expect("insert custom row");
        drop(first);

        let second = CatalogIndex::open(&path).expect("reopen");
        let value: Vec<u8> = second
            .conn
            .query_row(
                "select value from schema_meta where key = 'custom'",
                [],
                |row| row.get(0),
            )
            .expect("custom row");
        assert_eq!(value, b"kept");
    }

    #[test]
    fn future_schema_version_is_rejected() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let path = temp.path().join("rem-state.sqlite");
        let conn = Connection::open(&path).expect("open raw sqlite");
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .expect("set future version");
        drop(conn);

        let err = CatalogIndex::open(&path).expect_err("future schema must fail");
        assert!(err.to_string().contains("newer than supported"), "{err}");
    }

    #[test]
    fn read_only_open_validates_existing_schema() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let path = temp.path().join("rem-state.sqlite");
        let index = CatalogIndex::open(&path).expect("open writable");
        drop(index);

        let read_only = CatalogIndex::open_read_only(&path).expect("open read only");
        assert_eq!(read_only.quick_check().expect("quick check"), "ok");
        let busy_timeout_ms: u32 = read_only
            .conn
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("busy timeout");
        assert_eq!(busy_timeout_ms, 5000);

        let missing = temp.path().join("missing.sqlite");
        let err = CatalogIndex::open_read_only(&missing).expect_err("missing read-only db");
        assert!(
            err.to_string().contains("open sqlite"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn indexes_committed_tape_journal_projection() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [7u8; 16];
        let scheme = ParityScheme {
            id: SchemeId::new_static("test-scheme"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 3,
        };
        let state = CommittedState {
            entries: vec![
                TapeFileEntry {
                    tape_file_number: 1,
                    kind: TapeFileKind::Object,
                    block_count: 3,
                    physical_start_hint: Some(10),
                    object_id: Some("object-1".to_string()),
                    first_parity_data_ordinal: Some(0),
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: None,
                    bootstrap_object_row: None,
                },
                TapeFileEntry {
                    tape_file_number: 2,
                    kind: TapeFileKind::ParitySidecar,
                    block_count: 2,
                    physical_start_hint: Some(13),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: Some(0),
                    protected_ordinal_start: Some(0),
                    protected_ordinal_end_exclusive: Some(3),
                    canonical_metadata_hash: Some([9u8; 32]),
                    bootstrap_object_row: None,
                },
                TapeFileEntry {
                    tape_file_number: 3,
                    kind: TapeFileKind::ParityMap,
                    block_count: 1,
                    physical_start_hint: Some(15),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: Some(0),
                    protected_ordinal_start: Some(0),
                    protected_ordinal_end_exclusive: Some(3),
                    canonical_metadata_hash: Some([8u8; 32]),
                    bootstrap_object_row: None,
                },
                TapeFileEntry {
                    tape_file_number: 4,
                    kind: TapeFileKind::Bootstrap,
                    block_count: 1,
                    physical_start_hint: Some(16),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: Some([7u8; 32]),
                    bootstrap_object_row: None,
                },
            ],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        };

        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "camera.copy-a".to_string(),
                display_name: Some("Camera copy A".to_string()),
                copy_class: Some("copy-a".to_string()),
                content_class: Some("camera".to_string()),
                created_at_utc: Some("2026-05-28T09:00:00Z".to_string()),
            })
            .expect("project tape pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "ACM007L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision tape before assigning pool");
        index
            .project_tape_pool_membership(tape_uuid, "camera.copy-a")
            .expect("assign tape pool");

        let report = index
            .index_committed_tape_journal(
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: 4096,
                    scheme: Some(scheme),
                    journal_offset_bytes: 123,
                },
                &state,
            )
            .expect("index journal");

        assert!(!report.ingestion_pending);
        assert_eq!(report.tape_files_rebuilt, 4);
        assert_eq!(report.object_copies_rebuilt, 1);
        assert_eq!(
            index
                .conn
                .query_row(
                    "select state from tapes where tape_uuid = ?1",
                    params![tape_uuid.to_vec()],
                    |row| row.get::<_, String>(0),
                )
                .expect("tape row"),
            "ingested"
        );
        assert_eq!(
            index
                .conn
                .query_row("select count(*) from tape_files", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("tape file count"),
            4
        );
        assert_eq!(
            index
                .conn
                .query_row("select status from object_copies", [], |row| {
                    row.get::<_, String>(0)
                })
                .expect("object copy status"),
            "committed"
        );
        assert_eq!(
            index
                .conn
                .query_row("select pool_id from object_copies", [], |row| {
                    row.get::<_, String>(0)
                })
                .expect("object copy pool"),
            "camera.copy-a"
        );
        assert_eq!(
            index
                .conn
                .query_row(
                    "select origin_kind, format_id, native_object_id from catalog_units",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .expect("catalog unit row"),
            (
                "native_object".to_string(),
                "unknown".to_string(),
                "object-1".to_string()
            )
        );
        let pools = index.list_tape_pools().expect("list tape pools");
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].pool_id, "camera.copy-a");
        assert_eq!(pools[0].display_name.as_deref(), Some("Camera copy A"));
        assert_eq!(pools[0].copy_class.as_deref(), Some("copy-a"));
        assert_eq!(pools[0].content_class.as_deref(), Some("camera"));
        assert_eq!(
            index
                .get_tape_pool("camera.copy-a")
                .expect("get tape pool")
                .expect("pool exists"),
            pools[0]
        );

        let tapes = index.list_tapes(None).expect("list tapes");
        assert_eq!(tapes.len(), 1);
        assert_eq!(tapes[0].tape_uuid, tape_uuid.to_vec());
        assert_eq!(tapes[0].pool_id.as_deref(), Some("camera.copy-a"));
        assert_eq!(tapes[0].body_format, None);
        assert_eq!(tapes[0].block_size, Some(4096));
        assert_eq!(tapes[0].state, "ingested");
        assert_eq!(tapes[0].last_committed_tape_file, Some(4));
        assert_eq!(tapes[0].total_committed_ordinals, 3);
        assert_eq!(
            index
                .list_tapes(Some("camera.copy-a"))
                .expect("list pool tapes"),
            tapes
        );
        assert!(index
            .list_tapes(Some("camera.copy-b"))
            .expect("list empty pool")
            .is_empty());
        assert_eq!(
            index
                .get_tape(&tape_uuid)
                .expect("get tape")
                .expect("tape exists"),
            tapes[0]
        );
        let tape_files = index.list_tape_files(&tape_uuid).expect("list tape files");
        assert_eq!(tape_files.len(), 4);
        assert_eq!(tape_files[0].kind, "object");
        assert_eq!(tape_files[0].object_id.as_deref(), Some("object-1"));
        assert_eq!(tape_files[1].kind, "parity_sidecar");
        assert_eq!(tape_files[2].kind, "parity_map");
        assert_eq!(tape_files[3].kind, "bootstrap");

        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "camera.copy-b".to_string(),
                display_name: Some("Camera copy B".to_string()),
                copy_class: Some("copy-b".to_string()),
                content_class: Some("camera".to_string()),
                created_at_utc: None,
            })
            .expect("project second tape pool");
        let err = index
            .project_tape_pool_membership(tape_uuid, "camera.copy-b")
            .expect_err("conflicting committed pool must fail");
        assert!(
            matches!(err, StateError::TapePoolAssignmentConflict(_)),
            "{err}"
        );
    }

    #[test]
    fn committed_tape_journal_ingest_preserves_sealed_tape_state() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, state) = rebuild_fixture();
        let scheme = input.scheme.clone().expect("fixture has parity scheme");

        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: input.tape_uuid,
                voltag: "ACM007L9".to_string(),
                block_size: input.block_size,
                parity: ParityConfig::Scheme(scheme),
                force: false,
            })
            .expect("provision tape");
        index.seal_tape(input.tape_uuid).expect("seal tape");

        index
            .index_committed_tape_journal(input.clone(), &state)
            .expect("ingest committed journal");

        let (state, last_file): (String, Option<i64>) = index
            .conn
            .query_row(
                "select state, last_committed_tape_file from tapes where tape_uuid = ?1",
                params![input.tape_uuid.to_vec()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("tape row");

        assert_eq!(state, "sealed");
        assert_eq!(last_file, Some(4));
    }

    /// Provision the rebuild fixture's tape, ingest its journal so committed
    /// copies exist, and return the (input, state) pair for further steps.
    fn provisioned_committed_fixture(
        index: &mut CatalogIndex,
        voltag: &str,
    ) -> (TapeJournalIndexInput, CommittedState) {
        let (input, state) = rebuild_fixture();
        let scheme = input.scheme.clone().expect("fixture has parity scheme");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: input.tape_uuid,
                voltag: voltag.to_string(),
                block_size: input.block_size,
                parity: ParityConfig::Scheme(scheme),
                force: false,
            })
            .expect("provision tape");
        index
            .index_committed_tape_journal(input.clone(), &state)
            .expect("ingest committed journal");
        (input, state)
    }

    fn tape_state_and_voltag(
        index: &CatalogIndex,
        tape_uuid: [u8; 16],
    ) -> (String, Option<String>) {
        index
            .conn
            .query_row(
                "select state, voltag from tapes where tape_uuid = ?1",
                params![tape_uuid.to_vec()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("tape row")
    }

    fn copy_statuses(index: &CatalogIndex, tape_uuid: [u8; 16]) -> Vec<String> {
        let mut stmt = index
            .conn
            .prepare("select status from object_copies where tape_uuid = ?1 order by status")
            .expect("prepare copy status query");
        let statuses = stmt
            .query_map(params![tape_uuid.to_vec()], |row| row.get(0))
            .expect("query copy statuses")
            .collect::<Result<Vec<String>, _>>()
            .expect("collect copy statuses");
        statuses
    }

    #[test]
    fn retire_tape_sets_terminal_state_releases_voltag_and_marks_copies_missing() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, state) = rebuild_fixture();
        let scheme = input.scheme.clone().expect("fixture has parity scheme");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: input.tape_uuid,
                voltag: "RMJ101L9".to_string(),
                block_size: input.block_size,
                parity: ParityConfig::Scheme(scheme),
                force: false,
            })
            .expect("provision tape");
        index
            .upsert_tape_pool_projection(pool_projection("camera.copy-a"))
            .expect("project tape pool");
        index
            .project_tape_pool_membership(input.tape_uuid, "camera.copy-a")
            .expect("assign tape to pool");
        index
            .index_committed_tape_journal(input.clone(), &state)
            .expect("ingest committed journal");

        let outcome = index
            .retire_tape(RetireTapeInput {
                tape_uuid: input.tape_uuid,
                reason: "recycled".to_string(),
            })
            .expect("retire tape");

        assert_eq!(
            outcome,
            RetireTapeOutcome {
                newly_retired: true,
                released_voltag: Some("RMJ101L9".to_string()),
                copies_marked_missing: 1,
            }
        );
        let tape = index
            .get_tape(&input.tape_uuid)
            .expect("get retired tape")
            .expect("retired row survives");
        assert_eq!(tape.state, "retired");
        assert_eq!(tape.voltag, None, "voltag must detach for rebind");
        assert_eq!(
            tape.pool_id.as_deref(),
            Some("camera.copy-a"),
            "pool membership is kept as history"
        );
        assert_eq!(copy_statuses(&index, input.tape_uuid), vec!["missing"]);
        assert_eq!(
            index
                .list_objects_with_no_committed_copies()
                .expect("degraded objects"),
            vec!["object-1".to_string()]
        );
    }

    #[test]
    fn retire_tape_rerun_is_idempotent_noop() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, _state) = provisioned_committed_fixture(&mut index, "RMJ102L9");
        index
            .retire_tape(RetireTapeInput {
                tape_uuid: input.tape_uuid,
                reason: "recycled".to_string(),
            })
            .expect("first retire");

        let rerun = index
            .retire_tape(RetireTapeInput {
                tape_uuid: input.tape_uuid,
                reason: "recycled".to_string(),
            })
            .expect("idempotent re-retire");

        assert_eq!(
            rerun,
            RetireTapeOutcome {
                newly_retired: false,
                released_voltag: None,
                copies_marked_missing: 0,
            }
        );
        let (state, voltag) = tape_state_and_voltag(&index, input.tape_uuid);
        assert_eq!(state, "retired");
        assert_eq!(voltag, None);
    }

    #[test]
    fn retire_tape_unknown_uuid_errors() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");

        let err = index
            .retire_tape(RetireTapeInput {
                tape_uuid: [0xEE; 16],
                reason: "recycled".to_string(),
            })
            .expect_err("unknown tape must fail");

        assert!(
            matches!(err, StateError::IndexCorrupt(ref message)
                if message.contains("cannot retire unknown tape")),
            "{err}"
        );
    }

    #[test]
    fn provision_tape_refuses_retired_row_even_with_force() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, _state) = provisioned_committed_fixture(&mut index, "RMJ103L9");
        index
            .retire_tape(RetireTapeInput {
                tape_uuid: input.tape_uuid,
                reason: "recycled".to_string(),
            })
            .expect("retire tape");

        for force in [false, true] {
            let err = index
                .provision_tape(ProvisionTapeInput {
                    tape_uuid: input.tape_uuid,
                    voltag: "RMJ103L9".to_string(),
                    block_size: input.block_size,
                    parity: ParityConfig::None,
                    force,
                })
                .expect_err("retired row must refuse re-provisioning");
            assert!(
                matches!(err, StateError::TapeProvisionConflict(ref message)
                    if message.contains("retired identities are permanent")),
                "force={force}: {err}"
            );
        }

        // The released barcode binds to a brand-new identity instead.
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: [0x99; 16],
                voltag: "RMJ103L9".to_string(),
                block_size: input.block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("fresh identity reuses the released barcode");
        let (state, voltag) = tape_state_and_voltag(&index, [0x99; 16]);
        assert_eq!(state, "ready");
        assert_eq!(voltag.as_deref(), Some("RMJ103L9"));
        let (state, voltag) = tape_state_and_voltag(&index, input.tape_uuid);
        assert_eq!(state, "retired");
        assert_eq!(voltag, None);
    }

    /// §10.2 resurrection-trap regression. This test must fail if any one of
    /// the four rebuild-preservation changes is reverted:
    /// 1. the `retired` arm of the preserve predicate,
    /// 2. the `retired` arm of the preserved-column merge,
    /// 3. the `retired` arms of both journal/bundle ingest state CASEs,
    /// 4. the post-merge `missing` copy re-derivation.
    #[test]
    fn rebuild_preserves_retired_tape_and_rederives_missing_copies() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        // No pool membership on purpose: a pool_id would keep the row
        // preserved through the `pool_id is not null` arm and mask a revert
        // of the `state = 'retired'` preserve-predicate arm.
        let (input, state) = provisioned_committed_fixture(&mut index, "RMJ104L9");
        index
            .retire_tape(RetireTapeInput {
                tape_uuid: input.tape_uuid,
                reason: "vtl-rebuilt".to_string(),
            })
            .expect("retire tape");

        // Changes 1 + 4: the 3c journal stays on disk as authoritative
        // history, so a full rebuild re-ingests it; the identity must stay
        // dead and its copies must be re-derived to `missing`.
        index
            .rebuild_from_authoritative_sources(
                &[],
                &[RebuildTapeJournalInput {
                    input: input.clone(),
                    state: state.clone(),
                }],
            )
            .expect("rebuild with retired tape journal");
        let (tape_state, voltag) = tape_state_and_voltag(&index, input.tape_uuid);
        assert_eq!(tape_state, "retired", "rebuild resurrected the identity");
        assert_eq!(voltag, None, "rebuild re-attached the released voltag");
        assert_eq!(
            copy_statuses(&index, input.tape_uuid),
            vec!["missing"],
            "rebuild did not re-derive copy statuses from the retired state"
        );

        // Change 3 (journal CASE): the live ingest path runs without the
        // rebuild's merge pass, so the CASE itself must keep the state.
        index
            .index_committed_tape_journal(input.clone(), &state)
            .expect("live journal ingest");
        let (tape_state, _) = tape_state_and_voltag(&index, input.tape_uuid);
        assert_eq!(tape_state, "retired", "live journal ingest un-retired");

        // Change 3 (bundle CASE, defense in depth): selection blocks writes
        // to retired tapes, but the projection must not trust that.
        index
            .project_committed_tape_file_bundle(
                input.clone(),
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 9,
                        kind: TapeFileKind::Object,
                        block_count: 1,
                        physical_start_hint: None,
                        object_id: None,
                        first_parity_data_ordinal: Some(3),
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: None,
                    }],
                    highest_protected_ordinal: 3,
                    total_committed_ordinals: 4,
                },
            )
            .expect("live bundle projection");
        let (tape_state, _) = tape_state_and_voltag(&index, input.tape_uuid);
        assert_eq!(tape_state, "retired", "live bundle projection un-retired");

        // Change 2: drive the merge pass alone against a journal-derived
        // row, simulating the rebuild-internal moment between ingest and
        // merge (this is the layer that survives even if the ingest CASE
        // regresses).
        index
            .conn
            .execute(
                "update tapes set state = 'ingested' where tape_uuid = ?1",
                params![input.tape_uuid.to_vec()],
            )
            .expect("force journal-derived state");
        let preserved = PreservedTapeRow {
            tape_uuid: input.tape_uuid.to_vec(),
            voltag: None,
            pool_id: None,
            block_size: Some(i64::from(input.block_size)),
            scheme_id: None,
            data_blocks_per_stripe: None,
            parity_blocks_per_stripe: None,
            stripes_per_neighborhood: None,
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
            last_committed_tape_file: None,
            state: "retired".to_string(),
            updated_at_utc: "2026-06-10T00:00:00Z".to_string(),
        };
        let tx = index.conn.transaction().expect("begin merge transaction");
        merge_preserved_tape_operator_columns_tx(&tx, &[preserved]).expect("merge retired row");
        tx.commit().expect("commit merge transaction");
        let (tape_state, _) = tape_state_and_voltag(&index, input.tape_uuid);
        assert_eq!(tape_state, "retired", "merge did not re-apply retired");
    }

    #[test]
    fn reconcile_tape_files_projection_preserves_object_copies() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, state) = rebuild_fixture();
        index
            .index_committed_tape_journal(input.clone(), &state)
            .expect("seed committed projection");

        let replacement = vec![
            TapeFileEntry {
                tape_file_number: 1,
                kind: TapeFileKind::Object,
                block_count: 5,
                physical_start_hint: None,
                object_id: None,
                first_parity_data_ordinal: Some(0),
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: None,
                bootstrap_object_row: None,
            },
            TapeFileEntry {
                tape_file_number: 2,
                kind: TapeFileKind::Bootstrap,
                block_count: 1,
                physical_start_hint: None,
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: None,
                bootstrap_object_row: None,
            },
        ];

        let report = index
            .reconcile_tape_files_projection(input.tape_uuid, &replacement, 0, 5)
            .expect("reconcile structural tape files");

        assert_eq!(report.tape_files_rebuilt, 2);
        assert_eq!(report.object_copies_rebuilt, 0);
        let files = index
            .list_tape_files(&input.tape_uuid)
            .expect("list reconciled tape files");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].block_count, 5);
        assert_eq!(files[0].object_id, None);
        assert_eq!(
            index
                .conn
                .query_row("select count(*) from object_copies", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("object copy count"),
            1
        );
    }

    #[test]
    fn object_copy_upsert_preserves_existing_pool_snapshot() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, state) = rebuild_fixture();
        index
            .index_committed_tape_journal(input.clone(), &state)
            .expect("seed committed projection");
        index
            .conn
            .execute("update object_copies set pool_id = 'pool.at.commit'", [])
            .expect("seed copy pool snapshot");
        index
            .conn
            .execute(
                "update tapes set pool_id = 'pool.after.commit' where tape_uuid = ?1",
                params![input.tape_uuid.to_vec()],
            )
            .expect("simulate later tape pool drift");

        let report = index
            .project_committed_tape_file_bundle(
                input.clone(),
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: state.entries.clone(),
                    highest_protected_ordinal: state.highest_protected_ordinal,
                    total_committed_ordinals: state.total_committed_ordinals,
                },
            )
            .expect("project committed bundle again");

        assert_eq!(report.object_copies_rebuilt, 1);
        assert_eq!(
            index
                .conn
                .query_row("select pool_id from object_copies", [], |row| {
                    row.get::<_, String>(0)
                })
                .expect("object copy pool"),
            "pool.at.commit"
        );
    }

    #[test]
    fn combined_native_object_and_bundle_projection_rolls_back_on_bundle_error() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [6u8; 16];
        let scheme = ParityScheme {
            id: SchemeId::new_static("test-scheme"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 3,
        };
        let object_id = "object-atomic";
        let err = index
            .project_native_object_and_committed_tape_file_bundle(
                NativeObjectProjectionInput {
                    object_id: object_id.to_string(),
                    caller_object_id: Some("caller-atomic".to_string()),
                    body_format: "rao-v1".to_string(),
                    logical_size_bytes: Some(42),
                    content_hash: Some(vec![1u8; 32]),
                    metadata_hash: Some(vec![2u8; 32]),
                    created_at_utc: Some("2026-05-28T10:00:00Z".to_string()),
                },
                &[],
                &[NativeObjectCopyProjectionInput {
                    object_id: object_id.to_string(),
                    tape_uuid,
                    tape_file_number: 1,
                    first_body_lba: 5,
                    first_parity_data_ordinal: Some(0),
                    protected_until_ordinal: Some(3),
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                    key_id: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x32; 32]),
                    stored_digest: Some(vec![0x32; 32]),
                }],
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: 4096,
                    scheme: Some(scheme),
                    journal_offset_bytes: 0,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 1,
                        kind: TapeFileKind::Object,
                        block_count: u64::MAX,
                        physical_start_hint: Some(0),
                        object_id: Some(object_id.to_string()),
                        first_parity_data_ordinal: Some(0),
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: None,
                    }],
                    highest_protected_ordinal: 3,
                    total_committed_ordinals: 3,
                },
            )
            .expect_err("bad bundle input must abort combined projection");

        assert!(matches!(err, StateError::IndexMigrationFailed(_)), "{err}");
        assert!(index
            .get_native_object(object_id)
            .expect("query native object")
            .is_none());
        assert!(index
            .find_native_object_copies(object_id)
            .expect("query native object copies")
            .is_empty());
        assert_eq!(
            index
                .conn
                .query_row("select count(*) from tape_files", [], |row| {
                    row.get::<_, u64>(0)
                })
                .expect("tape file count"),
            0
        );
        assert_eq!(
            index
                .conn
                .query_row("select count(*) from tapes", [], |row| row.get::<_, u64>(0))
                .expect("tape count"),
            0
        );
    }

    #[test]
    fn native_object_projection_populates_catalog_units_and_queries() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [9u8; 16];

        index
            .upsert_native_object_projection(
                NativeObjectProjectionInput {
                    object_id: "object-1".to_string(),
                    caller_object_id: Some("caller-1".to_string()),
                    body_format: "rao-v1".to_string(),
                    logical_size_bytes: Some(42),
                    content_hash: Some(vec![1u8; 32]),
                    metadata_hash: Some(vec![2u8; 32]),
                    created_at_utc: Some("2026-05-28T10:00:00Z".to_string()),
                },
                &[NativeObjectCopyProjectionInput {
                    object_id: "object-1".to_string(),
                    tape_uuid,
                    tape_file_number: 9,
                    first_body_lba: 0,
                    first_parity_data_ordinal: Some(3),
                    protected_until_ordinal: Some(10),
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                    key_id: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x33; 32]),
                    stored_digest: Some(vec![0x33; 32]),
                }],
            )
            .expect("project native object");

        let objects = index.list_native_objects().expect("list native objects");
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].object_id, "object-1");
        assert_eq!(objects[0].caller_object_id.as_deref(), Some("caller-1"));
        assert_eq!(objects[0].body_format, "rao-v1");
        assert_eq!(objects[0].logical_size_bytes, Some(42));
        assert_eq!(objects[0].copies.len(), 1);
        assert_eq!(objects[0].copies[0].tape_file_number, 9);
        assert_eq!(objects[0].copies[0].first_parity_data_ordinal, Some(3));
        assert_eq!(objects[0].copies[0].pool_id, None);

        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "late.pool".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project late pool");
        let err = index
            .project_tape_pool_membership(tape_uuid, "late.pool")
            .expect_err("committed unknown-pool data must block assignment");
        assert!(
            matches!(err, StateError::TapePoolAssignmentConflict(_)),
            "{err}"
        );

        let fetched = index
            .get_native_object("object-1")
            .expect("get native object")
            .expect("native object exists");
        assert_eq!(fetched, objects[0]);
        assert_eq!(
            index
                .get_native_object_by_content_hash(&[1u8; 32])
                .expect("get native object by hash")
                .expect("native object exists by hash"),
            objects[0]
        );
        assert_eq!(
            index
                .get_native_object_by_caller_object_id("caller-1")
                .expect("get native object by caller id")
                .expect("native object exists by caller id"),
            objects[0]
        );
        assert_eq!(
            index
                .find_native_object_copies("object-1")
                .expect("find object copies"),
            objects[0].copies
        );
        let mut streamed_objects = Vec::new();
        index
            .for_each_native_object(|object| {
                streamed_objects.push(object);
                ControlFlow::Continue(())
            })
            .expect("stream native objects");
        assert_eq!(streamed_objects, objects);

        let units = index
            .list_catalog_units(CatalogUnitFilter::All)
            .expect("list catalog units");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].origin_kind, "native_object");
        assert_eq!(units[0].format_id, "rao-v1");
        assert_eq!(units[0].native_object_id.as_deref(), Some("object-1"));
        assert_eq!(units[0].tape_uuid, tape_uuid.to_vec());
        assert_eq!(
            index
                .list_catalog_units(CatalogUnitFilter::NativeObjects)
                .expect("list native units"),
            units
        );
        assert!(index
            .list_catalog_units(CatalogUnitFilter::ForeignArchives)
            .expect("list foreign units")
            .is_empty());
        let mut streamed_units = Vec::new();
        index
            .for_each_catalog_unit(CatalogUnitFilter::All, |unit| {
                streamed_units.push(unit);
                ControlFlow::Continue(())
            })
            .expect("stream catalog units");
        assert_eq!(streamed_units, units);

        let unit_id = native_catalog_unit_id("object-1", tape_uuid, 9);
        assert_eq!(
            index.get_catalog_unit(&unit_id).expect("get catalog unit"),
            Some(units[0].clone())
        );
    }

    #[test]
    fn encrypted_native_object_copy_projection_persists_envelope_fields() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [12u8; 16];
        let scheme = provision_scheme();
        let object_id = "encrypted-object-1";
        let key_id = vec![0x24u8; 16];

        index
            .project_native_object_and_committed_tape_file_bundle(
                NativeObjectProjectionInput {
                    object_id: object_id.to_string(),
                    caller_object_id: Some("caller-encrypted".to_string()),
                    body_format: "rao-v1".to_string(),
                    logical_size_bytes: Some(4096),
                    content_hash: Some(vec![0x11u8; 32]),
                    metadata_hash: None,
                    created_at_utc: Some("2026-06-11T19:00:00Z".to_string()),
                },
                &[],
                &[NativeObjectCopyProjectionInput {
                    object_id: object_id.to_string(),
                    tape_uuid,
                    tape_file_number: 2,
                    first_body_lba: 7,
                    first_parity_data_ordinal: Some(24),
                    protected_until_ordinal: Some(31),
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_ENCRYPTED.to_string(),
                    key_id: Some(key_id.clone()),
                    metadata_frame_len: Some(66),
                    plaintext_digest: Some(vec![0x44; 32]),
                    stored_digest: Some(vec![0x55; 32]),
                }],
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: 4096,
                    scheme: Some(scheme),
                    journal_offset_bytes: 0,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 2,
                        kind: TapeFileKind::Object,
                        block_count: 4,
                        physical_start_hint: Some(100),
                        object_id: Some(object_id.to_string()),
                        first_parity_data_ordinal: Some(24),
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: None,
                    }],
                    highest_protected_ordinal: 31,
                    total_committed_ordinals: 4,
                },
            )
            .expect("project encrypted object copy");

        let copies = index
            .find_native_object_copies(object_id)
            .expect("find encrypted object copies");
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].first_body_lba, 7);
        assert_eq!(
            copies[0].representation,
            OBJECT_COPY_REPRESENTATION_ENCRYPTED
        );
        assert_eq!(copies[0].key_id.as_deref(), Some(key_id.as_slice()));
        assert_eq!(copies[0].metadata_frame_len, Some(66));
        assert_eq!(copies[0].plaintext_digest.as_deref(), Some(&[0x44; 32][..]));
        assert_eq!(copies[0].stored_digest.as_deref(), Some(&[0x55; 32][..]));

        let (representation, stored_key_id, metadata_frame_len, plaintext_digest, stored_digest) =
            index
                .conn
                .query_row(
                    "select representation, key_id, metadata_frame_len,
                        plaintext_digest, stored_digest
                 from object_copies where object_id = ?1",
                    params![object_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Vec<u8>>(3)?,
                            row.get::<_, Vec<u8>>(4)?,
                        ))
                    },
                )
                .expect("read raw object copy row");
        assert_eq!(representation, OBJECT_COPY_REPRESENTATION_ENCRYPTED);
        assert_eq!(stored_key_id, key_id);
        assert_eq!(metadata_frame_len, 66);
        assert_eq!(plaintext_digest, vec![0x44; 32]);
        assert_eq!(stored_digest, vec![0x55; 32]);
    }

    #[test]
    fn encrypted_object_copy_projection_rejects_metadata_frame_len_out_of_bounds() {
        for metadata_frame_len in [0, 16, 16 * 1024 * 1024 + 1] {
            let err = validate_object_copy_envelope(
                Some(OBJECT_COPY_REPRESENTATION_ENCRYPTED),
                Some(&[0x24; 16]),
                Some(metadata_frame_len),
            )
            .unwrap_err();

            assert!(
                err.to_string().contains("metadata_frame_len"),
                "{metadata_frame_len}: {err}"
            );
        }
    }

    #[test]
    fn journal_entry_without_bootstrap_row_projects_unknown_representation() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [14u8; 16];
        let object_id = "journal-unknown-object";

        index
            .index_committed_tape_journal(
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: 4096,
                    scheme: Some(provision_scheme()),
                    journal_offset_bytes: 43,
                },
                &CommittedState {
                    entries: vec![TapeFileEntry {
                        tape_file_number: 6,
                        kind: TapeFileKind::Object,
                        block_count: 7,
                        physical_start_hint: Some(800),
                        object_id: Some(object_id.to_string()),
                        first_parity_data_ordinal: Some(19),
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: None,
                    }],
                    highest_protected_ordinal: 26,
                    total_committed_ordinals: 26,
                },
            )
            .expect("index journal without bootstrap object row");

        let copies = index
            .find_native_object_copies(object_id)
            .expect("find object copies");
        assert_eq!(copies.len(), 1);
        assert_eq!(copies[0].representation, OBJECT_COPY_REPRESENTATION_UNKNOWN);
        assert!(copies[0].key_id.is_none());
        assert!(copies[0].metadata_frame_len.is_none());
    }

    #[test]
    fn journal_bootstrap_object_row_projects_encrypted_copy_fields() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [13u8; 16];
        let key_id = [0x35u8; 16];
        let object_id = "journal-encrypted-object";

        index
            .index_committed_tape_journal(
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: 4096,
                    scheme: Some(provision_scheme()),
                    journal_offset_bytes: 42,
                },
                &CommittedState {
                    entries: vec![TapeFileEntry {
                        tape_file_number: 5,
                        kind: TapeFileKind::Object,
                        block_count: 9,
                        physical_start_hint: Some(700),
                        object_id: Some(object_id.to_string()),
                        first_parity_data_ordinal: Some(17),
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: Some(BootstrapObjectRow::encrypted(5, 9, key_id, 66)),
                    }],
                    highest_protected_ordinal: 26,
                    total_committed_ordinals: 26,
                },
            )
            .expect("index journal with encrypted bootstrap object row");

        let copies = index
            .find_native_object_copies(object_id)
            .expect("find object copies");
        assert_eq!(copies.len(), 1);
        assert_eq!(
            copies[0].representation,
            OBJECT_COPY_REPRESENTATION_ENCRYPTED
        );
        assert_eq!(copies[0].key_id.as_deref(), Some(key_id.as_slice()));
        assert_eq!(copies[0].metadata_frame_len, Some(66));
    }

    #[test]
    fn object_projection_refreshes_journal_discovered_catalog_unit_format() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, state) = rebuild_fixture();
        let tape_uuid = input.tape_uuid;

        index
            .index_committed_tape_journal(input, &state)
            .expect("index journal");
        let unit_id = native_catalog_unit_id("object-1", tape_uuid, 1);
        assert_eq!(
            index
                .get_catalog_unit(&unit_id)
                .expect("get journal-created unit")
                .expect("unit exists")
                .format_id,
            "unknown"
        );

        index
            .upsert_native_object_projection(
                NativeObjectProjectionInput {
                    object_id: "object-1".to_string(),
                    caller_object_id: None,
                    body_format: "rao-v1".to_string(),
                    logical_size_bytes: Some(99),
                    content_hash: None,
                    metadata_hash: None,
                    created_at_utc: Some("2026-05-28T10:05:00Z".to_string()),
                },
                &[NativeObjectCopyProjectionInput {
                    object_id: "object-1".to_string(),
                    tape_uuid,
                    tape_file_number: 1,
                    first_body_lba: 0,
                    first_parity_data_ordinal: Some(0),
                    protected_until_ordinal: Some(3),
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                    key_id: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x34; 32]),
                    stored_digest: Some(vec![0x34; 32]),
                }],
            )
            .expect("project object details");

        assert_eq!(
            index
                .get_catalog_unit(&unit_id)
                .expect("get refreshed unit")
                .expect("unit exists")
                .format_id,
            "rao-v1"
        );
    }

    #[test]
    fn foreign_archive_projection_populates_catalog_units() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");

        let unit_id = index
            .upsert_foreign_archive_projection(ForeignArchiveProjectionInput {
                tape_uuid: Vec::new(),
                format_id: "remanence-bru".to_string(),
                scan_id: "scan-1".to_string(),
                source_kind: "byte_stream_dump".to_string(),
                source_id: "dump:/tmp/archive.bru".to_string(),
                confidence: "high".to_string(),
                entry_count: 3,
                damage_event_count: 1,
                adapter_state: vec![0xab, 0xcd],
                last_scan_at_utc: Some("2026-05-28T13:00:00Z".to_string()),
                created_at_utc: Some("2026-05-28T13:00:01Z".to_string()),
            })
            .expect("project foreign archive");

        let units = index
            .list_catalog_units(CatalogUnitFilter::ForeignArchives)
            .expect("list foreign units");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].unit_id, unit_id);
        assert_eq!(units[0].origin_kind, "foreign_archive");
        assert_eq!(units[0].format_id, "remanence-bru");
        assert_eq!(units[0].scan_id.as_deref(), Some("scan-1"));
        assert_eq!(units[0].source_kind.as_deref(), Some("byte_stream_dump"));
        assert_eq!(units[0].source_id.as_deref(), Some("dump:/tmp/archive.bru"));
        assert_eq!(units[0].confidence.as_deref(), Some("high"));
        assert_eq!(units[0].entry_count, Some(3));
        assert_eq!(units[0].damage_event_count, Some(1));
        assert_eq!(units[0].adapter_state, vec![0xab, 0xcd]);
        assert_eq!(
            index.get_catalog_unit(&unit_id).expect("get foreign unit"),
            Some(units[0].clone())
        );
    }

    #[test]
    fn marks_tape_journal_ingestion_pending() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let tape_uuid = [8u8; 16];
        let scheme = ParityScheme {
            id: SchemeId::new_static("test-scheme"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 3,
        };

        let report = index
            .mark_tape_journal_ingestion_pending(tape_uuid, 4096, &scheme)
            .expect("mark pending");

        assert!(report.ingestion_pending);
        assert_eq!(
            index
                .conn
                .query_row(
                    "select state from tapes where tape_uuid = ?1",
                    params![tape_uuid.to_vec()],
                    |row| row.get::<_, String>(0),
                )
                .expect("tape row"),
            "ingestion_pending"
        );
    }

    #[test]
    fn wiping_sqlite_and_rebuilding_from_journal_state_is_equivalent() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let path = temp.path().join("rem-state.sqlite");
        let (input, state) = rebuild_fixture();
        let expected = {
            let mut index = CatalogIndex::open(&path).expect("open original");
            index
                .index_committed_tape_journal(input.clone(), &state)
                .expect("index original");
            catalog_snapshot(&index)
        };

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(sqlite_sidecar(&path, "-wal"));
        let _ = fs::remove_file(sqlite_sidecar(&path, "-shm"));

        let mut rebuilt = CatalogIndex::open(&path).expect("open rebuilt");
        let report = rebuilt
            .rebuild_from_authoritative_sources(
                &[],
                &[RebuildTapeJournalInput {
                    input,
                    state: state.clone(),
                }],
            )
            .expect("rebuild");

        assert_eq!(report.tapes_rebuilt, 1);
        assert_eq!(report.tape_files_rebuilt, 4);
        assert_eq!(report.object_copies_rebuilt, 1);
        assert_eq!(report.audit_records_replayed, 0);
        assert_eq!(report.journal_records_replayed, 1);
        assert_eq!(catalog_snapshot(&rebuilt), expected);
    }

    #[test]
    fn full_rebuild_preserves_provisioning_columns_and_unwritten_tapes() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, state) = rebuild_fixture();
        let written_tape = input.tape_uuid;
        let unwritten_tape = [0x77; 16];

        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: written_tape,
                voltag: "RMN101L9".to_string(),
                block_size: input.block_size,
                parity: ParityConfig::Scheme(input.scheme.clone().unwrap()),
                force: false,
            })
            .expect("provision written tape");
        index
            .upsert_tape_pool_projection(pool_projection("camera.copy-a"))
            .expect("project tape pool");
        index
            .project_tape_pool_membership(written_tape, "camera.copy-a")
            .expect("assign written tape to pool");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: unwritten_tape,
                voltag: "RMN102L9".to_string(),
                block_size: 262_144,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision unwritten tape");

        let report = index
            .rebuild_from_authoritative_sources(
                &[],
                &[RebuildTapeJournalInput {
                    input,
                    state: state.clone(),
                }],
            )
            .expect("rebuild with preserved provisioning");

        assert_eq!(report.tapes_rebuilt, 1);
        let written = index
            .get_tape(&written_tape)
            .expect("get written tape")
            .expect("written tape survives");
        assert_eq!(written.voltag.as_deref(), Some("RMN101L9"));
        assert_eq!(written.pool_id.as_deref(), Some("camera.copy-a"));
        assert_eq!(written.state, "ready");
        assert_eq!(
            written.total_committed_ordinals,
            state.total_committed_ordinals
        );

        let unwritten = index
            .get_tape(&unwritten_tape)
            .expect("get unwritten tape")
            .expect("unwritten provisioned tape survives");
        assert_eq!(unwritten.voltag.as_deref(), Some("RMN102L9"));
        assert_eq!(unwritten.state, "ready");
        assert_eq!(unwritten.total_committed_ordinals, 0);
        assert_eq!(unwritten.last_committed_tape_file, None);
    }

    #[test]
    fn failed_full_rebuild_rolls_back_prior_projection() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let (input, state) = rebuild_fixture();
        index
            .rebuild_from_authoritative_sources(
                &[],
                &[RebuildTapeJournalInput {
                    input,
                    state: state.clone(),
                }],
            )
            .expect("initial rebuild");
        let expected = catalog_snapshot(&index);
        let idempotency_key = Uuid::from_u128(0xCC);
        let bad_records = vec![audit_record(
            1,
            AuditEvent::RequestReceived,
            None,
            None,
            Some(idempotency_key),
            "object",
            detail(&[("request_fingerprint", CborValue::Bytes(vec![1]))]),
        )];

        let err = index
            .rebuild_from_authoritative_sources(&bad_records, &[])
            .expect_err("malformed idempotency replay must fail");

        assert!(matches!(err, StateError::IndexMigrationFailed(_)), "{err}");
        assert_eq!(catalog_snapshot(&index), expected);
    }

    #[test]
    fn request_received_projects_queued_operation() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let operation_id = Uuid::from_u128(0xABCD);
        let idempotency_key = Uuid::from_u128(0xCCDD);
        let record = audit_record(
            1,
            AuditEvent::RequestReceived,
            Some(operation_id),
            None,
            Some(idempotency_key),
            "write_object",
            detail(&[
                (
                    "operation_kind",
                    CborValue::Text("write_object".to_string()),
                ),
                ("request_fingerprint", CborValue::Bytes(vec![1, 2, 3])),
            ]),
        );

        index
            .project_audit_record(&record)
            .expect("project request received");

        let operation = index
            .get_operation(&operation_id.to_string())
            .expect("get operation")
            .expect("operation exists");
        assert_eq!(operation.operation_kind, "write_object");
        assert_eq!(operation.state, "queued");
        assert_eq!(operation.started_at_utc, "2026-05-27T10:01:00Z");
        assert_eq!(operation.updated_at_utc, "2026-05-27T10:01:00Z");
        let non_terminal = index
            .non_terminal_operations()
            .expect("non-terminal operations");
        assert_eq!(non_terminal.len(), 1);
        assert_eq!(non_terminal[0].operation_id, operation_id);
        assert_eq!(non_terminal[0].idempotency_key, Some(idempotency_key));
    }

    #[test]
    fn audit_replay_projects_operations_sessions_and_idempotency() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let operation_id = Uuid::from_u128(0xAA);
        let session_id = Uuid::from_u128(0xBB);
        let idempotency_key = Uuid::from_u128(0xCC);
        let tape_uuid = vec![7u8; 16];
        let request_fingerprint = vec![1, 2, 3, 4];
        let response_fingerprint = vec![5, 6, 7, 8];
        let records = vec![
            audit_record(
                1,
                AuditEvent::SessionOpened,
                None,
                Some(session_id),
                None,
                "write",
                detail(&[
                    ("session_kind", CborValue::Text("write".to_string())),
                    ("tape_uuid", CborValue::Bytes(tape_uuid.clone())),
                    ("library_serial", CborValue::Text("LIB001".to_string())),
                    ("drive_bay", CborValue::Integer(3.into())),
                ]),
            ),
            audit_record(
                2,
                AuditEvent::RequestReceived,
                Some(operation_id),
                Some(session_id),
                Some(idempotency_key),
                "object",
                detail(&[
                    (
                        "operation_kind",
                        CborValue::Text("write_object".to_string()),
                    ),
                    (
                        "request_fingerprint",
                        CborValue::Bytes(request_fingerprint.clone()),
                    ),
                ]),
            ),
            audit_record(
                3,
                AuditEvent::OperationStarted,
                Some(operation_id),
                Some(session_id),
                Some(idempotency_key),
                "object",
                detail(&[(
                    "operation_kind",
                    CborValue::Text("write_object".to_string()),
                )]),
            ),
            audit_record(
                4,
                AuditEvent::OperationFinished,
                Some(operation_id),
                Some(session_id),
                Some(idempotency_key),
                "object",
                detail(&[(
                    "response_fingerprint",
                    CborValue::Bytes(response_fingerprint.clone()),
                )]),
            ),
            audit_record(
                5,
                AuditEvent::SessionClosed,
                None,
                Some(session_id),
                None,
                "write",
                BTreeMap::new(),
            ),
        ];

        let report = index.replay_audit_records(&records).expect("replay audit");

        assert_eq!(report.audit_records_replayed, 5);
        assert_eq!(report.operations_rebuilt, 1);
        assert_eq!(report.sessions_rebuilt, 1);
        assert_eq!(report.idempotency_keys_rebuilt, 1);
        assert_eq!(
            index
                .conn
                .query_row(
                    "select operation_kind, state, session_id, subject from operations",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .expect("operation row"),
            (
                "write_object".to_string(),
                "finished".to_string(),
                session_id.to_string(),
                "object:subject-1".to_string()
            )
        );
        let operations = index.list_operations().expect("list operations");
        assert_eq!(operations.len(), 1);
        assert_eq!(operations[0].operation_id, operation_id.to_string());
        assert_eq!(operations[0].operation_kind, "write_object");
        assert_eq!(operations[0].state, "finished");
        let session_id_text = session_id.to_string();
        assert_eq!(
            operations[0].session_id.as_deref(),
            Some(session_id_text.as_str())
        );
        assert_eq!(operations[0].subject.as_deref(), Some("object:subject-1"));
        assert_eq!(operations[0].started_at_utc, "2026-05-27T10:02:00Z");
        assert_eq!(operations[0].updated_at_utc, "2026-05-27T10:04:00Z");
        assert_eq!(
            index
                .get_operation(&operation_id.to_string())
                .expect("get operation"),
            Some(operations[0].clone())
        );
        assert_eq!(
            index
                .conn
                .query_row(
                    "select session_kind, tape_uuid, library_serial, drive_bay, state from sessions",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .expect("session row"),
            (
                "write".to_string(),
                tape_uuid,
                "LIB001".to_string(),
                3,
                "closed".to_string()
            )
        );
        assert_eq!(
            index
                .conn
                .query_row(
                    "select actor_fingerprint, request_fingerprint, operation_id, terminal_state, response_fingerprint
                     from idempotency_keys",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Vec<u8>>(4)?,
                        ))
                    },
                )
                .expect("idempotency row"),
            (
                "user:alice".to_string(),
                request_fingerprint,
                operation_id.to_string(),
                "finished".to_string(),
                response_fingerprint
            )
        );
    }

    #[test]
    fn incremental_audit_projection_rejects_idempotency_request_conflict() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let idempotency_key = Uuid::from_u128(0xCC);
        let first = audit_record(
            1,
            AuditEvent::RequestReceived,
            Some(Uuid::from_u128(1)),
            None,
            Some(idempotency_key),
            "object",
            detail(&[("request_fingerprint", CborValue::Bytes(vec![1]))]),
        );
        let second = audit_record(
            2,
            AuditEvent::RequestReceived,
            Some(Uuid::from_u128(2)),
            None,
            Some(idempotency_key),
            "object",
            detail(&[("request_fingerprint", CborValue::Bytes(vec![2]))]),
        );

        index
            .project_audit_record(&first)
            .expect("first idempotency request");
        let err = index
            .project_audit_record(&second)
            .expect_err("live conflicting idempotency request must fail");

        assert!(matches!(err, StateError::IdempotencyConflict(_)), "{err}");
    }

    #[test]
    fn audit_replay_preserves_first_idempotency_request_on_conflict() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-index")
            .tempdir()
            .expect("temp dir");
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("open");
        let idempotency_key = Uuid::from_u128(0xCC);
        let first_operation_id = Uuid::from_u128(1);
        let records = vec![
            audit_record(
                1,
                AuditEvent::RequestReceived,
                Some(first_operation_id),
                None,
                Some(idempotency_key),
                "object",
                detail(&[("request_fingerprint", CborValue::Bytes(vec![1]))]),
            ),
            audit_record(
                2,
                AuditEvent::RequestReceived,
                Some(Uuid::from_u128(2)),
                None,
                Some(idempotency_key),
                "object",
                detail(&[("request_fingerprint", CborValue::Bytes(vec![2]))]),
            ),
        ];

        let report = index
            .replay_audit_records(&records)
            .expect("historical idempotency conflict should not brick replay");

        assert_eq!(report.idempotency_keys_rebuilt, 1);
        assert_eq!(
            index
                .conn
                .query_row(
                    "select request_fingerprint, operation_id
                     from idempotency_keys
                     where actor_fingerprint = 'user:alice' and idempotency_key = ?1",
                    params![idempotency_key.to_string()],
                    |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
                )
                .expect("idempotency row"),
            (vec![1], first_operation_id.to_string())
        );
    }
}
