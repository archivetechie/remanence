//! Public Layer 4 state handle.

use std::collections::BTreeMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ciborium::value::Value as CborValue;
use remanence_parity::{FileTapeFileJournal, ParityScheme};
use time::Duration;

use crate::audit::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSink, AuditSubject, FileAuditLog, SourceLayer,
};
use crate::config::{load_config, validate_trusted_volume_paths, RemConfig};
use crate::error::StateError;
use crate::index::{
    AuditReplayReport, CatalogIndex, RebuildReport, RebuildTapeJournalInput, RetireTapeInput,
    RetireTapeOutcome, TapeJournalIndexInput, TapeJournalIndexReport, TapePoolProjectionInput,
};
use crate::lock::StateLockGuard;
use crate::paths::StatePaths;

/// Open Layer 4 state owner.
#[derive(Debug)]
pub struct StateHandle {
    paths: StatePaths,
    config: RemConfig,
    config_warnings: Vec<StateConfigWarning>,
    _lock: StateLockGuard,
    audit: FileAuditLog,
    index: CatalogIndex,
}

/// Non-fatal configuration condition observed while opening state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateConfigWarning {
    /// Tape pools are configured, but no rules can assign tapes to them.
    TapePoolsWithoutRules {
        /// Number of configured tape pools that will be unreachable by rules.
        pool_count: usize,
    },
}

/// Result of attempting to ingest one tape journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TapeJournalIngestionOutcome {
    /// Journal replay completed and SQLite was updated.
    Indexed(TapeJournalIndexReport),
    /// A live append session owns the 3c journal lock; retry later.
    Pending(TapeJournalIndexReport),
}

/// Report from startup replay and restart cleanup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupReplayReport {
    /// Full rebuild report from audit logs and tape journals.
    pub rebuild: RebuildReport,
    /// Number of non-terminal operations marked failed after restart.
    pub lost_operations_marked: u64,
    /// Number of non-terminal sessions marked lost after restart.
    pub lost_sessions_marked: u64,
}

impl StateHandle {
    /// Open state by loading config and acquiring the exclusive state lock.
    pub fn open_from_config_file(config_path: impl AsRef<Path>) -> Result<Self, StateError> {
        let config_path = config_path.as_ref();
        let config = load_config(config_path)?;
        let paths = StatePaths::from_config(config_path, &config);
        Self::open_with_config(paths, config)
    }

    /// Reset the local rebuildable catalog state while preserving the operator config.
    ///
    /// Active audit segments and Layer 3c journals are first archived under
    /// `state_dir/reset-archives/`; derived SQLite/cache files are discarded.
    /// The schema is then recreated and configured tape pools are projected into
    /// an otherwise empty catalog.
    pub fn reset_catalog_from_config_file(config_path: impl AsRef<Path>) -> Result<(), StateError> {
        let config_path = config_path.as_ref();
        let config = load_config(config_path)?;
        let paths = StatePaths::from_config(config_path, &config);
        Self::reset_catalog_with_config(paths, config)
    }

    /// Open state with already-resolved paths and a parsed config.
    pub fn open_with_config(paths: StatePaths, config: RemConfig) -> Result<Self, StateError> {
        let lock = StateLockGuard::acquire(&paths.state_dir)?;
        ensure_state_directories(&paths)?;
        validate_trusted_volume_paths(&config)?;
        let audit = FileAuditLog::open_with_clock_forward_tolerance(
            &paths.audit_dir,
            config.audit.fsync,
            Some(Duration::seconds(
                config.audit.clock_forward_tolerance_seconds as i64,
            )),
        )?;
        let mut index = CatalogIndex::open(&paths.sqlite_path)?;
        let config_warnings = project_configured_tape_pools(&mut index, &config)?;
        index.reconcile_cleaning_prefixes(&config.cleaning.voltag_prefixes)?;
        Ok(Self {
            paths,
            config,
            config_warnings,
            _lock: lock,
            audit,
            index,
        })
    }

    /// Reset local catalog state with already-resolved paths and parsed config.
    pub fn reset_catalog_with_config(
        paths: StatePaths,
        config: RemConfig,
    ) -> Result<(), StateError> {
        let _lock = StateLockGuard::acquire(&paths.state_dir)?;
        archive_reset_authoritative_inputs(&paths)?;
        reset_directory_contents(&paths.audit_dir)?;
        reset_directory_contents(&paths.journal_dir)?;
        reset_directory_contents(&paths.tape_cache_dir)?;
        remove_sqlite_file_and_sidecars(&paths.sqlite_path)?;
        ensure_state_directories(&paths)?;
        let mut index = CatalogIndex::open(&paths.sqlite_path)?;
        let _config_warnings = project_configured_tape_pools(&mut index, &config)?;
        index.reconcile_cleaning_prefixes(&config.cleaning.voltag_prefixes)?;
        Ok(())
    }

    /// Return parsed operator config.
    pub fn config(&self) -> &RemConfig {
        &self.config
    }

    /// Return non-fatal configuration warnings observed while opening state.
    pub fn config_warnings(&self) -> &[StateConfigWarning] {
        &self.config_warnings
    }

    /// Return concrete state paths.
    pub fn paths(&self) -> &StatePaths {
        &self.paths
    }

    /// Return the mutable audit sink.
    pub fn audit(&mut self) -> &mut dyn AuditSink {
        &mut self.audit
    }

    /// Return the mutable catalog projection owner.
    pub fn catalog_index(&mut self) -> &mut CatalogIndex {
        &mut self.index
    }

    /// Replay the authoritative audit log into SQLite-derived projections.
    pub fn replay_audit_projection(&mut self) -> Result<AuditReplayReport, StateError> {
        let records = FileAuditLog::replay(&self.paths.audit_dir)?;
        self.index.replay_audit_records(&records)
    }

    /// Rebuild the SQLite projection from audit logs and all local 3c journals.
    pub fn rebuild_index_from_journals(&mut self) -> Result<RebuildReport, StateError> {
        let audit_records = FileAuditLog::replay(&self.paths.audit_dir)?;
        let tape_journals = self.load_tape_journal_rebuild_inputs()?;
        self.index
            .rebuild_from_authoritative_sources(&audit_records, &tape_journals)
    }

    /// Run startup replay and mark non-terminal prior work as lost by restart.
    pub fn startup_replay(&mut self) -> Result<StartupReplayReport, StateError> {
        let rebuild = self.rebuild_index_from_journals()?;
        let lost_operations_marked = self.mark_lost_operations_by_restart()?;
        let lost_sessions_marked = self.mark_lost_sessions_by_restart()?;
        Ok(StartupReplayReport {
            rebuild,
            lost_operations_marked,
            lost_sessions_marked,
        })
    }

    /// Retire one tape identity in the catalog and audit the transition.
    ///
    /// Ordering note: catalog first, audit second. A failed audit append
    /// surfaces as an error but does not roll back the retire — the same
    /// crash window exists for every audited mutation in the codebase today,
    /// and audit-before-commit would invert the lie (an audit record for a
    /// retire that never happened).
    pub fn retire_tape(&mut self, input: RetireTapeInput) -> Result<RetireTapeOutcome, StateError> {
        let tape_uuid = input.tape_uuid;
        let reason = input.reason.clone();
        let outcome = self.index.retire_tape(input)?;
        // An idempotent rerun changed nothing, so it appends nothing: the
        // `TapeRetired` event is the tamper-evident record of who declared
        // the medium dead, when, and why — that declaration already exists.
        if outcome.newly_retired {
            let mut detail = BTreeMap::new();
            detail.insert(
                "voltag".to_string(),
                outcome
                    .released_voltag
                    .clone()
                    .map(CborValue::Text)
                    .unwrap_or(CborValue::Null),
            );
            detail.insert("reason".to_string(), CborValue::Text(reason));
            detail.insert(
                "copies_marked_missing".to_string(),
                CborValue::Integer(outcome.copies_marked_missing.into()),
            );
            self.audit.append(AuditEventRecord {
                actor: AuditActor::local_user(),
                source_layer: SourceLayer::Layer4,
                operation_id: None,
                session_id: None,
                idempotency_key: None,
                event: AuditEvent::TapeRetired,
                subject: AuditSubject {
                    kind: "tape".to_string(),
                    id: Some(hex_tape_uuid(tape_uuid)),
                },
                detail,
            })?;
        }
        Ok(outcome)
    }

    /// Return the Layer 3c journal path for a tape UUID.
    pub fn journal_path(&self, tape_uuid: [u8; 16]) -> PathBuf {
        self.paths.journal_path(tape_uuid)
    }

    /// Replay one Layer 3c journal through the 3c shared reader and index it.
    pub fn ingest_tape_journal(
        &mut self,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<TapeJournalIngestionOutcome, StateError> {
        let path = self.journal_path(tape_uuid);
        let reader = match FileTapeFileJournal::open_shared_for_replay(
            &path,
            tape_uuid,
            block_size,
            scheme.clone(),
        ) {
            Ok(reader) => reader,
            Err(err) if err.is_lock_contended() => {
                let report = self
                    .index
                    .mark_tape_journal_ingestion_pending(tape_uuid, block_size, &scheme)?;
                return Ok(TapeJournalIngestionOutcome::Pending(report));
            }
            Err(err) => {
                return Err(StateError::JournalReplayFailed(format!(
                    "open shared journal replay {}: {err}",
                    path.display()
                )));
            }
        };

        let state = reader.load_committed().map_err(|err| {
            StateError::JournalReplayFailed(format!(
                "load committed journal {}: {err}",
                path.display()
            ))
        })?;
        let journal_offset_bytes = fs::metadata(&path)
            .map_err(|err| StateError::io_at("stat ingested journal", &path, err))?
            .len();
        let report = self.index.index_committed_tape_journal(
            TapeJournalIndexInput {
                tape_uuid,
                block_size,
                scheme: Some(scheme),
                journal_offset_bytes,
            },
            &state,
        )?;
        Ok(TapeJournalIngestionOutcome::Indexed(report))
    }

    fn load_tape_journal_rebuild_inputs(&self) -> Result<Vec<RebuildTapeJournalInput>, StateError> {
        if !self.paths.journal_dir.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(&self.paths.journal_dir).map_err(|err| {
            StateError::io_at("read journal directory", &self.paths.journal_dir, err)
        })? {
            let entry = entry.map_err(|err| {
                StateError::io_at("read journal directory entry", &self.paths.journal_dir, err)
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("remjournal") {
                paths.push(path);
            }
        }
        paths.sort();

        let mut inputs = Vec::with_capacity(paths.len());
        for path in paths {
            let reader = match FileTapeFileJournal::open_shared_existing_for_replay(&path) {
                Ok(reader) => reader,
                Err(err) if err.is_lock_contended() => {
                    return Err(StateError::IndexRebuildInProgress);
                }
                Err(err) => {
                    return Err(StateError::JournalReplayFailed(format!(
                        "open shared journal replay {}: {err}",
                        path.display()
                    )));
                }
            };
            let journal_offset_bytes = fs::metadata(&path)
                .map_err(|err| StateError::io_at("stat ingested journal", &path, err))?
                .len();
            let input = TapeJournalIndexInput {
                tape_uuid: reader.tape_uuid(),
                block_size: reader.block_size(),
                scheme: Some(reader.scheme().clone()),
                journal_offset_bytes,
            };
            let state = reader.load_committed().map_err(|err| {
                StateError::JournalReplayFailed(format!(
                    "load committed journal {}: {err}",
                    path.display()
                ))
            })?;
            inputs.push(RebuildTapeJournalInput { input, state });
        }
        Ok(inputs)
    }

    fn mark_lost_operations_by_restart(&mut self) -> Result<u64, StateError> {
        let operations = self.index.non_terminal_operations()?;
        let count = operations.len() as u64;
        for operation in operations {
            let mut detail = BTreeMap::new();
            detail.insert(
                "operation_kind".to_string(),
                CborValue::Text(operation.operation_kind.clone()),
            );
            detail.insert(
                "restart_reason".to_string(),
                CborValue::Text("daemon_restart".to_string()),
            );
            if let Some(subject) = operation.subject.as_ref() {
                detail.insert(
                    "previous_subject".to_string(),
                    CborValue::Text(subject.clone()),
                );
            }
            if let Some(actor_fingerprint) = operation.actor_fingerprint.as_ref() {
                detail.insert(
                    "actor_fingerprint".to_string(),
                    CborValue::Text(actor_fingerprint.clone()),
                );
            }
            let subject_kind = operation
                .subject
                .as_deref()
                .unwrap_or(operation.operation_kind.as_str())
                .to_string();
            let (_, record) = self.audit.append_and_return_record(AuditEventRecord {
                actor: AuditActor::System,
                source_layer: SourceLayer::Layer4,
                operation_id: Some(operation.operation_id),
                session_id: operation.session_id,
                idempotency_key: operation.idempotency_key,
                event: AuditEvent::OperationFailed,
                subject: AuditSubject {
                    kind: subject_kind,
                    id: Some(operation.operation_id.to_string()),
                },
                detail,
            })?;
            self.index.project_audit_record(&record)?;
        }
        Ok(count)
    }

    fn mark_lost_sessions_by_restart(&mut self) -> Result<u64, StateError> {
        let sessions = self.index.non_terminal_sessions()?;
        let count = sessions.len() as u64;
        for session in sessions {
            let mut detail = BTreeMap::new();
            detail.insert(
                "session_kind".to_string(),
                CborValue::Text(session.session_kind.clone()),
            );
            detail.insert(
                "restart_reason".to_string(),
                CborValue::Text("daemon_restart".to_string()),
            );
            if let Some(tape_uuid) = session.tape_uuid.as_ref() {
                detail.insert("tape_uuid".to_string(), CborValue::Bytes(tape_uuid.clone()));
            }
            if let Some(library_serial) = session.library_serial.as_ref() {
                detail.insert(
                    "library_serial".to_string(),
                    CborValue::Text(library_serial.clone()),
                );
            }
            if let Some(drive_bay) = session.drive_bay {
                detail.insert(
                    "drive_bay".to_string(),
                    CborValue::Integer(drive_bay.into()),
                );
            }
            if let Some(drive_uuid) = session.drive_uuid.as_ref() {
                detail.insert(
                    "drive_uuid".to_string(),
                    CborValue::Bytes(drive_uuid.clone()),
                );
            }
            let (_, record) = self.audit.append_and_return_record(AuditEventRecord {
                actor: AuditActor::System,
                source_layer: SourceLayer::Layer4,
                operation_id: None,
                session_id: Some(session.session_id),
                idempotency_key: None,
                event: AuditEvent::SessionLostByRestart,
                subject: AuditSubject {
                    kind: session.session_kind,
                    id: Some(session.session_id.to_string()),
                },
                detail,
            })?;
            self.index.project_audit_record(&record)?;
        }
        Ok(count)
    }
}

fn hex_tape_uuid(tape_uuid: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in tape_uuid {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("write to string");
    }
    out
}

fn project_configured_tape_pools(
    index: &mut CatalogIndex,
    config: &RemConfig,
) -> Result<Vec<StateConfigWarning>, StateError> {
    let pools = config
        .tape_pools
        .iter()
        .map(|pool| TapePoolProjectionInput {
            pool_id: pool.id.clone(),
            display_name: pool.display_name.clone(),
            copy_class: pool.copy_class.clone(),
            content_class: pool.content_class.clone(),
            created_at_utc: None,
        })
        .collect::<Vec<_>>();
    let mut warnings = Vec::new();
    if config.tape_pool_rules.is_empty() && !config.tape_pools.is_empty() {
        warnings.push(StateConfigWarning::TapePoolsWithoutRules {
            pool_count: config.tape_pools.len(),
        });
    }
    index.reconcile_tape_pool_projection_from_rules(&pools, &config.tape_pool_rules)?;
    Ok(warnings)
}

fn ensure_state_directories(paths: &StatePaths) -> Result<(), StateError> {
    create_private_dir(&paths.audit_dir)?;
    create_private_dir(&paths.journal_dir)?;
    if let Some(parent) = paths.sqlite_path.parent() {
        create_private_dir(parent)?;
    }
    create_private_dir(&paths.tape_cache_dir)?;
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<(), StateError> {
    #[cfg(unix)]
    {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        builder.mode(0o700);
        builder
            .create(path)
            .map_err(|err| StateError::io_at("create state subdirectory", path, err))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|err| StateError::io_at("chmod state subdirectory", path, err))?;
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
            .map_err(|err| StateError::io_at("create state subdirectory", path, err))?;
    }

    Ok(())
}

fn reset_directory_contents(path: &Path) -> Result<(), StateError> {
    if path.exists() {
        fs::remove_dir_all(path)
            .map_err(|err| StateError::io_at("remove state subdirectory", path, err))?;
    }
    create_private_dir(path)
}

const RESET_ARCHIVE_DIR: &str = "reset-archives";

fn archive_reset_authoritative_inputs(paths: &StatePaths) -> Result<(), StateError> {
    if !paths.audit_dir.exists() && !paths.journal_dir.exists() {
        return Ok(());
    }

    let archive_dir = create_unique_reset_archive_dir(&paths.state_dir)?;
    archive_directory_if_exists(&paths.audit_dir, &archive_dir.join("audit"))?;
    archive_directory_if_exists(&paths.journal_dir, &archive_dir.join("journals"))?;
    Ok(())
}

fn create_unique_reset_archive_dir(state_dir: &Path) -> Result<PathBuf, StateError> {
    let root = state_dir.join(RESET_ARCHIVE_DIR);
    create_private_dir(&root)?;

    for index in 1..=999_999u32 {
        let candidate = root.join(format!("reset-{index:06}"));
        match fs::create_dir(&candidate) {
            Ok(()) => {
                #[cfg(unix)]
                fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700))
                    .map_err(|err| StateError::io_at("chmod reset archive", &candidate, err))?;
                return Ok(candidate);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(StateError::io_at(
                    "create reset archive directory",
                    &candidate,
                    err,
                ));
            }
        }
    }

    Err(StateError::ConfigInvalid(format!(
        "no free reset archive name under {}",
        root.display()
    )))
}

fn archive_directory_if_exists(source: &Path, destination: &Path) -> Result<(), StateError> {
    if source.exists() {
        copy_directory_recursive(source, destination)?;
    }
    Ok(())
}

fn copy_directory_recursive(source: &Path, destination: &Path) -> Result<(), StateError> {
    create_private_dir(destination)?;
    let entries =
        fs::read_dir(source).map_err(|err| StateError::io_at("read reset source", source, err))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| StateError::io_at("read reset source entry", source, err))?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|err| StateError::io_at("stat reset source entry", &source_path, err))?;
        if file_type.is_dir() {
            copy_directory_recursive(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)
                .map_err(|err| StateError::io_at("copy reset source file", &source_path, err))?;
        } else {
            return Err(StateError::ConfigInvalid(format!(
                "refusing to archive non-file state entry {}",
                source_path.display()
            )));
        }
    }
    Ok(())
}

fn remove_sqlite_file_and_sidecars(path: &Path) -> Result<(), StateError> {
    remove_file_if_exists(path)?;
    remove_file_if_exists(&path.with_file_name(format!(
        "{}-wal",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rem-state.sqlite")
    )))?;
    remove_file_if_exists(&path.with_file_name(format!(
        "{}-shm",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rem-state.sqlite")
    )))?;
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<(), StateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(StateError::io_at("remove sqlite file", path, err)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use ciborium::value::Value as CborValue;
    use remanence_parity::ParityConfig;
    use uuid::Uuid;

    use super::*;
    use crate::config::parse_config_toml;

    fn config_text(root: &Path) -> String {
        format!(
            r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
read_only = false

[[libraries]]
serial = "LIB001"

[[tape_pools]]
id = "camera.copy-a"
display_name = "Camera copy A"
copy_class = "copy-a"
content_class = "camera"

[[tape_pool_rules]]
prefix = "ACM"
pool_id = "camera.copy-a"

[journal]
dir = "{0}/journals"
require_trusted_volume = false

[audit]
dir = "{0}/audit"
fsync = true

[index]
sqlite_path = "{0}/index/rem-state.sqlite"

[cache]
tape_catalog_dir = "{0}/cache/tapes"
"#,
            root.display()
        )
    }

    fn config_without_pools(root: &Path) -> String {
        config_text(root).replace(
            r#"[[tape_pools]]
id = "camera.copy-a"
display_name = "Camera copy A"
copy_class = "copy-a"
content_class = "camera"

[[tape_pool_rules]]
prefix = "ACM"
pool_id = "camera.copy-a"

"#,
            "",
        )
    }

    fn config_with_pool_but_no_rules(root: &Path) -> String {
        config_text(root).replace(
            r#"[[tape_pool_rules]]
prefix = "ACM"
pool_id = "camera.copy-a"

"#,
            "",
        )
    }

    fn config_with_pool_b(root: &Path) -> String {
        config_text(root).replace("camera.copy-a", "camera.copy-b")
    }

    #[test]
    fn open_from_config_file_acquires_state_owner() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-handle")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");

        assert_eq!(handle.config().libraries[0].serial, "LIB001");
        assert_eq!(handle.config().tape_pools[0].id, "camera.copy-a");
        assert_eq!(handle.config().tape_pool_rules[0].pool_id, "camera.copy-a");
        assert!(handle.paths().state_dir.ends_with(temp.path()));
        assert!(temp.path().join("audit").is_dir());
        assert!(temp.path().join("journals").is_dir());
        assert!(temp.path().join("index").is_dir());
        assert!(temp.path().join("index/rem-state.sqlite").is_file());
        assert!(temp.path().join("cache/tapes").is_dir());
        assert_eq!(
            handle
                .catalog_index()
                .schema_version()
                .expect("schema version"),
            crate::index::SCHEMA_VERSION
        );
        assert_eq!(
            handle
                .catalog_index()
                .get_tape_pool("camera.copy-a")
                .expect("get configured pool")
                .expect("pool exists")
                .display_name
                .as_deref(),
            Some("Camera copy A")
        );
    }

    #[test]
    fn open_from_config_file_reports_pool_without_rules_warning() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-pool-warning")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_with_pool_but_no_rules(temp.path())).expect("write config");

        let handle = StateHandle::open_from_config_file(&config_path).expect("open state");

        assert_eq!(
            handle.config_warnings(),
            &[StateConfigWarning::TapePoolsWithoutRules { pool_count: 1 }]
        );
    }

    #[test]
    fn reset_catalog_preserves_config_and_clears_rebuildable_state() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-reset")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let tape_uuid = *Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("uuid")
            .as_bytes();

        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .catalog_index()
                .provision_tape(crate::index::ProvisionTapeInput {
                    tape_uuid,
                    voltag: "ACM001L9".to_string(),
                    block_size: 4096,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision tape");
        }

        fs::write(temp.path().join("audit/old.remaudit"), b"audit").expect("write audit");
        fs::write(temp.path().join("journals/old.remjournal"), b"journal").expect("write journal");
        fs::write(temp.path().join("cache/tapes/old.cache"), b"cache").expect("write cache");
        fs::write(temp.path().join("index/rem-state.sqlite-wal"), b"wal").expect("write wal");
        fs::write(temp.path().join("index/rem-state.sqlite-shm"), b"shm").expect("write shm");

        StateHandle::reset_catalog_from_config_file(&config_path).expect("reset catalog");

        assert!(config_path.is_file());
        assert!(temp.path().join("audit").is_dir());
        assert!(temp.path().join("journals").is_dir());
        assert!(temp.path().join("cache/tapes").is_dir());
        assert!(!temp.path().join("audit/old.remaudit").exists());
        assert!(!temp.path().join("journals/old.remjournal").exists());
        assert!(!temp.path().join("cache/tapes/old.cache").exists());
        assert!(!temp.path().join("index/rem-state.sqlite-wal").exists());
        assert!(!temp.path().join("index/rem-state.sqlite-shm").exists());
        let archive_dir = temp.path().join("reset-archives/reset-000001");
        assert_eq!(
            fs::read(archive_dir.join("audit/old.remaudit")).expect("archived audit"),
            b"audit"
        );
        assert_eq!(
            fs::read(archive_dir.join("journals/old.remjournal")).expect("archived journal"),
            b"journal"
        );
        assert!(
            !archive_dir.join("cache/tapes/old.cache").exists(),
            "derived cache must not be archived"
        );

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        assert!(handle
            .catalog_index()
            .list_tapes(None)
            .expect("list tapes")
            .is_empty());
        assert_eq!(
            handle
                .catalog_index()
                .get_tape_pool("camera.copy-a")
                .expect("pool lookup")
                .expect("pool projected")
                .display_name
                .as_deref(),
            Some("Camera copy A")
        );
    }

    #[test]
    fn reopen_reconciles_removed_config_tape_pools() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-handle")
            .tempdir()
            .expect("temp dir");
        let config = parse_config_toml(&config_text(temp.path())).expect("config");
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let tape_uuid = *Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("uuid")
            .as_bytes();

        {
            let mut handle =
                StateHandle::open_with_config(paths.clone(), config).expect("open with pool");
            handle
                .catalog_index()
                .provision_tape(crate::index::ProvisionTapeInput {
                    tape_uuid,
                    voltag: "ACM001L9".to_string(),
                    block_size: 4096,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("project tape");
        }

        {
            let config = parse_config_toml(&config_text(temp.path())).expect("config");
            let mut handle =
                StateHandle::open_with_config(paths.clone(), config).expect("reopen with pool");
            assert_eq!(
                handle
                    .catalog_index()
                    .list_tapes(Some("camera.copy-a"))
                    .expect("pool tapes")
                    .len(),
                1
            );
        }

        let config = parse_config_toml(&config_without_pools(temp.path())).expect("config");
        let mut handle = StateHandle::open_with_config(paths, config).expect("reopen without pool");

        assert!(handle
            .catalog_index()
            .get_tape_pool("camera.copy-a")
            .expect("pool lookup")
            .is_none());
        let tapes = handle.catalog_index().list_tapes(None).expect("list tapes");
        assert_eq!(tapes.len(), 1);
        assert_eq!(tapes[0].pool_id, None);
        assert!(handle
            .catalog_index()
            .list_tapes(Some("camera.copy-a"))
            .expect("pool tapes after removal")
            .is_empty());
    }

    #[test]
    fn reopen_reconciles_derived_pool_membership_from_rules() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-handle")
            .tempdir()
            .expect("temp dir");
        let config = parse_config_toml(&config_text(temp.path())).expect("config");
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let tape_uuid = *Uuid::parse_str("11111111-1111-1111-1111-111111111111")
            .expect("uuid")
            .as_bytes();

        {
            let mut handle =
                StateHandle::open_with_config(paths.clone(), config).expect("open with pool");
            handle
                .catalog_index()
                .provision_tape(crate::index::ProvisionTapeInput {
                    tape_uuid,
                    voltag: "ACM001L9".to_string(),
                    block_size: 4096,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("project tape");
        }

        let config = parse_config_toml(&config_with_pool_b(temp.path())).expect("config");
        let mut handle = StateHandle::open_with_config(paths, config).expect("reopen with pool b");
        let tapes = handle.catalog_index().list_tapes(None).expect("list tapes");
        assert_eq!(tapes.len(), 1);
        assert_eq!(tapes[0].pool_id.as_deref(), Some("camera.copy-b"));
        assert!(handle
            .catalog_index()
            .get_tape_pool("camera.copy-a")
            .expect("pool lookup")
            .is_none());
    }

    #[test]
    fn second_state_handle_is_rejected() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-handle")
            .tempdir()
            .expect("temp dir");
        let config = parse_config_toml(&config_text(temp.path())).expect("config");
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let _first =
            StateHandle::open_with_config(paths.clone(), config.clone()).expect("first handle");
        let err = StateHandle::open_with_config(paths, config).expect_err("second must fail");

        assert!(err.is_state_lock_held(), "{err}");
    }

    #[test]
    fn rebuild_index_from_empty_journal_dir_returns_zero_report() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-handle")
            .tempdir()
            .expect("temp dir");
        let config = parse_config_toml(&config_text(temp.path())).expect("config");
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let mut handle = StateHandle::open_with_config(paths, config).expect("open handle");

        let report = handle.rebuild_index_from_journals().expect("empty rebuild");

        assert_eq!(report.tapes_rebuilt, 0);
        assert_eq!(report.tape_files_rebuilt, 0);
        assert_eq!(report.object_copies_rebuilt, 0);
        assert_eq!(report.audit_records_replayed, 0);
        assert_eq!(report.journal_records_replayed, 0);
    }

    #[test]
    fn retire_tape_appends_audit_event_and_idempotent_rerun_appends_nothing() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-retire")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let tape_uuid = [0x5Eu8; 16];
        let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
        handle
            .catalog_index()
            .provision_tape(crate::index::ProvisionTapeInput {
                tape_uuid,
                voltag: "ACM001L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");

        let outcome = handle
            .retire_tape(crate::index::RetireTapeInput {
                tape_uuid,
                reason: "recycled".to_string(),
            })
            .expect("retire tape");

        assert!(outcome.newly_retired);
        assert_eq!(outcome.released_voltag.as_deref(), Some("ACM001L9"));
        assert_eq!(outcome.copies_marked_missing, 0);
        let records = FileAuditLog::replay(handle.paths().audit_dir.clone()).expect("replay audit");
        let retired = records
            .iter()
            .filter(|record| record.event == AuditEvent::TapeRetired)
            .collect::<Vec<_>>();
        assert_eq!(retired.len(), 1);
        let record = retired[0];
        assert_eq!(record.actor, AuditActor::local_user());
        assert_eq!(record.source_layer, SourceLayer::Layer4);
        assert_eq!(record.subject.kind, "tape");
        assert_eq!(
            record.subject.id.as_deref(),
            Some("5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e")
        );
        assert_eq!(
            record.detail.get("voltag"),
            Some(&CborValue::Text("ACM001L9".to_string()))
        );
        assert_eq!(
            record.detail.get("reason"),
            Some(&CborValue::Text("recycled".to_string()))
        );
        assert_eq!(
            record.detail.get("copies_marked_missing"),
            Some(&CborValue::Integer(0.into()))
        );

        // An idempotent rerun changed nothing and must append nothing: the
        // existing record already says who declared the medium dead.
        let rerun = handle
            .retire_tape(crate::index::RetireTapeInput {
                tape_uuid,
                reason: "recycled".to_string(),
            })
            .expect("idempotent re-retire");
        assert!(!rerun.newly_retired);
        let records =
            FileAuditLog::replay(handle.paths().audit_dir.clone()).expect("replay audit again");
        assert_eq!(
            records
                .iter()
                .filter(|record| record.event == AuditEvent::TapeRetired)
                .count(),
            1
        );
    }

    #[test]
    fn audit_replay_treats_retire_and_provision_events_as_inert() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-retire-inert")
            .tempdir()
            .expect("temp dir");
        let config = parse_config_toml(&config_text(temp.path())).expect("config");
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let session_id = Uuid::from_u128(0x61);
        let operation_id = Uuid::from_u128(0x62);

        {
            let mut handle =
                StateHandle::open_with_config(paths.clone(), config.clone()).expect("open first");
            handle
                .audit()
                .append(AuditEventRecord {
                    actor: AuditActor::User("alice".to_string()),
                    source_layer: SourceLayer::Layer5,
                    operation_id: None,
                    session_id: Some(session_id),
                    idempotency_key: None,
                    event: AuditEvent::SessionOpened,
                    subject: AuditSubject {
                        kind: "write".to_string(),
                        id: Some(session_id.to_string()),
                    },
                    detail: BTreeMap::from([(
                        "session_kind".to_string(),
                        CborValue::Text("write".to_string()),
                    )]),
                })
                .expect("append session opened");
            handle
                .audit()
                .append(AuditEventRecord {
                    actor: AuditActor::User("alice".to_string()),
                    source_layer: SourceLayer::Layer4,
                    operation_id: None,
                    session_id: None,
                    idempotency_key: None,
                    event: AuditEvent::TapeProvisioned,
                    subject: AuditSubject {
                        kind: "tape".to_string(),
                        id: Some("21".repeat(16)),
                    },
                    detail: BTreeMap::from([(
                        "voltag".to_string(),
                        CborValue::Text("ACM002L9".to_string()),
                    )]),
                })
                .expect("append tape provisioned");
            handle
                .audit()
                .append(AuditEventRecord {
                    actor: AuditActor::User("alice".to_string()),
                    source_layer: SourceLayer::Layer4,
                    operation_id: None,
                    session_id: None,
                    idempotency_key: None,
                    event: AuditEvent::TapeRetired,
                    subject: AuditSubject {
                        kind: "tape".to_string(),
                        id: Some("21".repeat(16)),
                    },
                    detail: BTreeMap::from([(
                        "reason".to_string(),
                        CborValue::Text("recycled".to_string()),
                    )]),
                })
                .expect("append tape retired");
            handle
                .audit()
                .append(AuditEventRecord {
                    actor: AuditActor::User("alice".to_string()),
                    source_layer: SourceLayer::Layer5,
                    operation_id: Some(operation_id),
                    session_id: Some(session_id),
                    idempotency_key: None,
                    event: AuditEvent::SessionClosed,
                    subject: AuditSubject {
                        kind: "write".to_string(),
                        id: Some(session_id.to_string()),
                    },
                    detail: BTreeMap::new(),
                })
                .expect("append session closed");
        }

        // Replay must round-trip the new event kinds from disk and project
        // operations/sessions exactly as if the new events were absent.
        let mut restarted =
            StateHandle::open_with_config(paths, config).expect("open restarted handle");
        let report = restarted.startup_replay().expect("startup replay");

        assert_eq!(report.rebuild.audit_records_replayed, 4);
        assert_eq!(report.lost_operations_marked, 0);
        assert_eq!(report.lost_sessions_marked, 0);
        assert_eq!(
            restarted
                .catalog_index()
                .session_state(session_id)
                .expect("session state")
                .as_deref(),
            Some("closed")
        );
    }

    #[test]
    fn startup_replay_marks_non_terminal_prior_work_lost() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-handle")
            .tempdir()
            .expect("temp dir");
        let config = parse_config_toml(&config_text(temp.path())).expect("config");
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let session_id = Uuid::from_u128(0x51);
        let operation_id = Uuid::from_u128(0x52);
        let idempotency_key = Uuid::from_u128(0x53);

        {
            let mut handle =
                StateHandle::open_with_config(paths.clone(), config.clone()).expect("open first");
            handle
                .audit()
                .append(AuditEventRecord {
                    actor: AuditActor::User("alice".to_string()),
                    source_layer: SourceLayer::Layer5,
                    operation_id: None,
                    session_id: Some(session_id),
                    idempotency_key: None,
                    event: AuditEvent::SessionOpened,
                    subject: AuditSubject {
                        kind: "write".to_string(),
                        id: Some(session_id.to_string()),
                    },
                    detail: BTreeMap::from([(
                        "session_kind".to_string(),
                        CborValue::Text("write".to_string()),
                    )]),
                })
                .expect("append session opened");
            handle
                .audit()
                .append(AuditEventRecord {
                    actor: AuditActor::User("alice".to_string()),
                    source_layer: SourceLayer::Layer5,
                    operation_id: Some(operation_id),
                    session_id: Some(session_id),
                    idempotency_key: Some(idempotency_key),
                    event: AuditEvent::RequestReceived,
                    subject: AuditSubject {
                        kind: "object".to_string(),
                        id: Some("object-1".to_string()),
                    },
                    detail: BTreeMap::from([(
                        "request_fingerprint".to_string(),
                        CborValue::Bytes(vec![1, 2, 3]),
                    )]),
                })
                .expect("append request received");
            handle
                .audit()
                .append(AuditEventRecord {
                    actor: AuditActor::User("alice".to_string()),
                    source_layer: SourceLayer::Layer5,
                    operation_id: Some(operation_id),
                    session_id: Some(session_id),
                    idempotency_key: Some(idempotency_key),
                    event: AuditEvent::OperationStarted,
                    subject: AuditSubject {
                        kind: "object".to_string(),
                        id: Some("object-1".to_string()),
                    },
                    detail: BTreeMap::from([(
                        "operation_kind".to_string(),
                        CborValue::Text("write_object".to_string()),
                    )]),
                })
                .expect("append operation started");
        }

        let mut restarted =
            StateHandle::open_with_config(paths.clone(), config).expect("open restarted");
        let report = restarted.startup_replay().expect("startup replay");

        assert_eq!(report.rebuild.audit_records_replayed, 3);
        assert_eq!(report.lost_operations_marked, 1);
        assert_eq!(report.lost_sessions_marked, 1);
        assert_eq!(
            restarted
                .catalog_index()
                .operation_state(operation_id)
                .expect("operation state")
                .as_deref(),
            Some("failed")
        );
        assert_eq!(
            restarted
                .catalog_index()
                .session_state(session_id)
                .expect("session state")
                .as_deref(),
            Some("lost_by_restart")
        );
        assert_eq!(
            restarted
                .catalog_index()
                .idempotency_terminal_state("user:alice", idempotency_key)
                .expect("idempotency terminal state")
                .as_deref(),
            Some("failed")
        );

        let records = FileAuditLog::replay(&paths.audit_dir).expect("replay audit");
        assert_eq!(records.len(), 5);
        assert!(records
            .iter()
            .any(|record| record.event == AuditEvent::SessionLostByRestart));
        assert!(records.iter().any(|record| {
            record.operation_id == Some(operation_id)
                && record.event == AuditEvent::OperationFailed
                && matches!(
                    record.detail.get("restart_reason"),
                    Some(CborValue::Text(value)) if value == "daemon_restart"
                )
        }));
    }
}
