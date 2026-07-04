//! Layer 5 gRPC service skeleton over the local Remanence state index.
//!
//! This crate owns the generated `proto/layer5.proto` bindings and the first
//! in-process service implementations. Daemon/catalog methods are backed by
//! `remanence-state::CatalogIndex`; read and write sessions dispatch to a
//! hardware-backed changer/drive actor pool when the daemon enables writes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ciborium::value::Value as CborValue;
#[cfg(feature = "foreign-bru")]
use remanence_bru::BruFormat;
#[cfg(feature = "foreign-bru")]
use remanence_format::error::FormatError;
#[cfg(feature = "foreign-bru")]
use remanence_format::{
    ArchiveGapCause, ArchiveGapRange, ArchiveReader, DamageRange, DamageStatus, EntryCatalogSink,
    EntryKind, NormalizedEntry,
};
use remanence_state::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSubject, CatalogIndex, CatalogUnitFilter,
    CatalogUnitRecord, DriveCorrelationRollupRecord, DriveHealthSnapshotInput, FileAuditLog,
    NativeObjectCopyRecord, NativeObjectFileRecord, NativeObjectRecord, OperationRecord, RemConfig,
    SourceLayer, StateError, TapeFileRecord, TapePoolConfig, TapePoolRecord, TapeRecord,
};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::transport::{Channel, Endpoint, Uri};
use tonic::{Request, Response, Status};
use uuid::Uuid;

pub mod pb {
    tonic::include_proto!("remanence.api.v1");
}

/// Connect a gRPC channel to a Unix-socket daemon (Layer 5 dev transport).
/// The URI authority is a placeholder ignored by the custom connector.
pub async fn connect_unix(socket_path: PathBuf) -> Result<Channel, tonic::transport::Error> {
    Endpoint::try_from("http://[::1]:50051")?
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

mod diagnostics;
mod library;
mod mount;
mod operations;
mod pool_selection;
mod pool_write;
pub mod read_core;
mod tape_init;
mod write_owner;

pub use library::LibraryServiceApi;
pub use mount::{load_tape_by_uuid, LoadByUuidError};
pub use pool_write::{
    build_tape_bootstrap, can_read, can_write, check_writability_preconditions,
    lto_generation_from_drive_product, lto_generation_from_voltag, raw_capacity_bytes,
    seal_decision_after_write, select_tape_in_pool, verify_tape_identity, write_object_to_pool,
    write_tape_bootstrap, write_to_selected_tape, LtoGen, PoolWriteError,
    PoolWriteObjectCopyRecord, PoolWriteObjectRecord, PoolWriteRepresentation, PoolWriteResult,
    SelectTapeError, SelectedTape, TapeIdentityError, TapePositionAfterWrite, TapeSealReason,
    TapeUuid, WritabilityError, WriteObjectToPoolRequest,
};
pub use remanence_library::{resolve_load_target, LoadError, LoadPlan};
pub use tape_init::{
    classify_bot_bytes, classify_bot_from_source, decide_tape_init,
    maybe_write_tape_init_bootstrap, project_tape_init_catalog_inputs, sniff,
    BarcodeLifecycleState, BotClassification, BotInitProjection, CatalogBarcodeRelation,
    CatalogRowDisposition, CatalogTapeInitRow, CommittedCopyState, FormatId, InitDecision,
    TapeInitCatalogProjection, TapeInitError, TapeInitGeometry, TapeInitWriteAction,
    TapeInitWriteError, TapeInitWriteOptions,
};

const CATALOG_STREAM_BUFFER: usize = 32;
type BytesChunkStream =
    Pin<Box<dyn Stream<Item = Result<pb::BytesChunk, Status>> + Send + 'static>>;

struct CountingBytesStream {
    inner: BytesChunkStream,
    state: ApiState,
    drive_uuid: Option<Vec<u8>>,
}

impl Stream for CountingBytesStream {
    type Item = Result<pb::BytesChunk, Status>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(chunk))) => {
                self.state
                    .record_drive_read_bytes(self.drive_uuid.as_deref(), chunk.data.len() as u64);
                std::task::Poll::Ready(Some(Ok(chunk)))
            }
            other => other,
        }
    }
}

/// Inventory snapshot captured once at daemon startup (S6a). Static until
/// RefreshInventory (S6b); LibraryState.last_inventory_at surfaces capture time.
pub(crate) struct LibrarySnapshot {
    pub(crate) report: remanence_library::DiscoveryReport,
    pub(crate) captured_at: OffsetDateTime,
}

#[derive(Debug)]
struct DriveByteCounters {
    read_bytes: AtomicU64,
    write_bytes: AtomicU64,
    counter_epoch: u64,
}

impl DriveByteCounters {
    fn new(counter_epoch: u64) -> Self {
        Self {
            read_bytes: AtomicU64::new(0),
            write_bytes: AtomicU64::new(0),
            counter_epoch,
        }
    }
}

#[derive(Debug)]
struct LiveStatusState {
    min_poll_interval: Duration,
    cache: RwLock<Option<(OffsetDateTime, pb::GetLiveStatusResponse)>>,
    drive_counters: RwLock<HashMap<Vec<u8>, Arc<DriveByteCounters>>>,
}

impl LiveStatusState {
    fn new(min_poll_interval: Duration) -> Self {
        Self {
            min_poll_interval,
            cache: RwLock::new(None),
            drive_counters: RwLock::new(HashMap::new()),
        }
    }

    fn counter_epoch(daemon_epoch: u64, drive_uuid: &[u8]) -> u64 {
        let mut hasher = Sha256::new();
        hasher.update(daemon_epoch.to_le_bytes());
        hasher.update(drive_uuid);
        let digest = hasher.finalize();
        u64::from_le_bytes(digest[..8].try_into().expect("sha256 prefix is 8 bytes"))
    }

    fn get_or_create_counters(
        &self,
        daemon_epoch: u64,
        drive_uuid: &[u8],
    ) -> Arc<DriveByteCounters> {
        if let Some(existing) = self
            .drive_counters
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .get(drive_uuid)
            .cloned()
        {
            return existing;
        }
        let mut counters = self
            .drive_counters
            .write()
            .unwrap_or_else(|err| err.into_inner());
        counters
            .entry(drive_uuid.to_vec())
            .or_insert_with(|| {
                Arc::new(DriveByteCounters::new(Self::counter_epoch(
                    daemon_epoch,
                    drive_uuid,
                )))
            })
            .clone()
    }
}

/// Shared state for the initial Layer 5 service implementations.
#[derive(Clone)]
pub struct ApiState {
    index_path: Arc<PathBuf>,
    audit_dir: Arc<PathBuf>,
    audit_fsync: bool,
    audit_append_lock: Arc<std::sync::Mutex<()>>,
    operations: crate::operations::OperationRegistry,
    pool_configs: Arc<HashMap<String, TapePoolConfig>>,
    managed_library_serials: Arc<HashSet<String>>,
    drive_pool: Option<crate::write_owner::DrivePool>,
    spool_dir: Option<Arc<PathBuf>>,
    spool_budget: Option<Arc<Semaphore>>,
    default_library_serial: Option<Arc<String>>,
    library_snapshot: Option<Arc<RwLock<Arc<LibrarySnapshot>>>>,
    live_status: Arc<LiveStatusState>,
    daemon_epoch: u64,
    daemon_version: String,
    api_version: String,
    rust_target: String,
}

impl ApiState {
    /// Build service state around an already-opened rebuildable catalog index.
    pub fn new(index: CatalogIndex) -> Self {
        Self::new_with_pool_configs(index, Vec::new())
    }

    /// Build service state with operator-resolved tape-pool selection config.
    pub fn new_with_config(index: CatalogIndex, config: &RemConfig) -> Self {
        let index_path = index.path().to_path_buf();
        let pool_configs = config
            .tape_pools
            .clone()
            .into_iter()
            .map(|pool| (pool.id.trim().to_string(), pool))
            .collect();
        Self::new_with_pool_configs_inner(
            index_path,
            pool_configs,
            // Configured-or-daemon-operated set (never raw config.drives —
            // its empty default would trip library_is_managed's empty⇒all
            // fallback and mark foreign libraries managed).
            drive_managed_library_serials(config),
            config.audit.dir.clone(),
            config.audit.fsync,
            Arc::new(std::sync::Mutex::new(())),
            live_status_config_from(&config.livestatus),
        )
    }

    /// Build service state with explicit tape-pool selection config.
    pub fn new_with_pool_configs(
        index: CatalogIndex,
        pool_configs: impl IntoIterator<Item = TapePoolConfig>,
    ) -> Self {
        let index_path = index.path().to_path_buf();
        let pool_configs = pool_configs
            .into_iter()
            .map(|pool| (pool.id.trim().to_string(), pool))
            .collect();
        let audit_dir = default_audit_dir_for_index(index_path.as_path());
        Self::new_with_pool_configs_inner(
            index_path,
            pool_configs,
            HashSet::new(),
            audit_dir,
            false,
            Arc::new(std::sync::Mutex::new(())),
            live_status_config_from(&remanence_state::LiveStatusConfig::default()),
        )
    }

    fn new_with_pool_configs_inner(
        index_path: PathBuf,
        pool_configs: HashMap<String, TapePoolConfig>,
        managed_library_serials: HashSet<String>,
        audit_dir: PathBuf,
        audit_fsync: bool,
        audit_append_lock: Arc<std::sync::Mutex<()>>,
        live_status_interval: Duration,
    ) -> Self {
        let daemon_epoch = Uuid::new_v4().as_u128() as u64;
        Self {
            index_path: Arc::new(index_path),
            audit_dir: Arc::new(audit_dir),
            audit_fsync,
            audit_append_lock,
            operations: crate::operations::OperationRegistry::default(),
            pool_configs: Arc::new(pool_configs),
            managed_library_serials: Arc::new(managed_library_serials),
            drive_pool: None,
            spool_dir: None,
            spool_budget: None,
            default_library_serial: None,
            library_snapshot: None,
            live_status: Arc::new(LiveStatusState::new(live_status_interval)),
            daemon_epoch,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            api_version: "v1-draft".to_string(),
            rust_target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
        }
    }

    /// Build service state with a live changer/drive actor pool.
    pub fn with_drive_pool(
        index: CatalogIndex,
        config: &RemConfig,
        report: remanence_library::DiscoveryReport,
        policy: remanence_library::StaticAllowlist,
        spool_dir: PathBuf,
    ) -> Result<Self, Status> {
        let index_path = index.path().to_path_buf();
        let pool_configs: HashMap<String, TapePoolConfig> = config
            .tape_pools
            .iter()
            .map(|pool| (pool.id.trim().to_string(), pool.clone()))
            .collect();
        let default_library_serial = match config.libraries.as_slice() {
            [library] => Some(Arc::new(library.serial.clone())),
            _ => None,
        };
        let audit_append_lock = Arc::new(std::sync::Mutex::new(()));
        let library_snapshot = Arc::new(RwLock::new(Arc::new(LibrarySnapshot {
            report: report.clone(),
            captured_at: OffsetDateTime::now_utc(),
        })));
        let library_serial = default_library_serial.as_ref().ok_or_else(|| {
            Status::invalid_argument(
                "drive-pool daemon mode requires exactly one configured library in this slice",
            )
        })?;
        let lib = report.library(library_serial.as_str()).ok_or_else(|| {
            Status::not_found(format!(
                "library {} not found in discovery report",
                library_serial.as_str()
            ))
        })?;
        let mut library = lib
            .open(&policy)
            .map_err(|err| Status::internal(format!("open library: {err}")))?;
        let mut opened_drives = Vec::new();
        for bay in library.library().drive_bays.clone() {
            let Some(installed) = bay.installed.as_ref() else {
                continue;
            };
            if installed.sg_path.is_none() {
                continue;
            }
            let bay_addr = bay.element_address;
            let drive = library.open_drive(bay_addr, &policy).map_err(|err| {
                Status::internal(format!("open drive bay 0x{bay_addr:04x}: {err}"))
            })?;
            opened_drives.push((bay_addr, drive));
        }
        if opened_drives.is_empty() {
            return Err(Status::failed_precondition(
                "configured library has no openable drives",
            ));
        }
        let reservations = Arc::new(
            opened_drives
                .iter()
                .map(|(bay, _)| (*bay, AtomicBool::new(false)))
                .collect::<HashMap<_, _>>(),
        );
        let managed_library_serials = drive_managed_library_serials(config);
        let base_cfg = crate::write_owner::WriteOwnerConfig {
            index_path: index_path.clone(),
            report: report.clone(),
            policy,
            audit_dir: config.audit.dir.clone(),
            audit_fsync: config.audit.fsync,
            audit_append_lock: audit_append_lock.clone(),
            reservations: reservations.clone(),
            default_library_serial: default_library_serial
                .as_ref()
                .map(|serial| serial.as_str().to_string()),
            library_snapshot: library_snapshot.clone(),
            snapshot_miss_alarm: config.drives.snapshot_miss_alarm,
            managed_library_serials: Arc::new(managed_library_serials),
            cleaning: config.cleaning.clone(),
        };
        let mut drive_txs = HashMap::new();
        for (bay_addr, drive) in opened_drives {
            let tx = crate::write_owner::spawn_drive_actor(bay_addr, drive, base_cfg.clone());
            drive_txs.insert(bay_addr, tx);
        }
        let changer_tx =
            crate::write_owner::spawn_changer_actor(library.into_changer(), base_cfg.clone());
        let drive_pool =
            crate::write_owner::DrivePool::new(changer_tx, drive_txs, reservations.clone());
        let mut state = Self::new_with_pool_configs_inner(
            index_path.clone(),
            pool_configs,
            // Same rule as the write_owner cfg above: configured-or-daemon-
            // operated, never the raw (default-empty) config list — empty
            // trips library_is_managed's empty⇒all fallback.
            drive_managed_library_serials(config),
            config.audit.dir.clone(),
            config.audit.fsync,
            audit_append_lock,
            live_status_config_from(&config.livestatus),
        );
        state.drive_pool = Some(drive_pool.clone());
        state.spool_dir = Some(Arc::new(spool_dir));
        state.spool_budget = Some(Arc::new(Semaphore::new(spool_budget_permits(
            crate::write_owner::SPOOL_MAX_BYTES,
        ) as usize)));
        state.default_library_serial = default_library_serial;
        state.library_snapshot = Some(library_snapshot);
        state.reconcile_drive_catalog_from_report(config, &report)?;
        state.reconcile_clean_runs_from_report(&report)?;
        spawn_drive_collection_workers(index_path, report, config, drive_pool);
        Ok(state)
    }

    /// Return the daemon service implementation.
    pub fn daemon_service(&self) -> DaemonService {
        DaemonService {
            state: self.clone(),
        }
    }

    /// Return the catalog service implementation.
    pub fn catalog_service(&self) -> CatalogService {
        CatalogService {
            state: self.clone(),
        }
    }

    /// Return the write-session service implementation.
    pub fn write_session_service(&self) -> WriteSessionApi {
        WriteSessionApi {
            state: self.clone(),
        }
    }

    /// Return the read-session service implementation.
    pub fn read_session_service(&self) -> ReadSessionApi {
        ReadSessionApi {
            state: self.clone(),
        }
    }

    /// Return the library-inspection service implementation.
    pub fn library_service(&self) -> LibraryServiceApi {
        LibraryServiceApi {
            state: self.clone(),
        }
    }

    fn index(&self) -> Result<CatalogIndex, Status> {
        CatalogIndex::open_read_only(self.index_path.as_ref())
            .map_err(|err| Status::internal(err.to_string()))
    }

    pub(crate) fn index_write(&self) -> Result<CatalogIndex, Status> {
        CatalogIndex::open(self.index_path.as_ref())
            .map_err(|err| Status::internal(err.to_string()))
    }

    /// Current inventory snapshot. S6b republishes into the shared cell.
    pub(crate) fn current_library_snapshot(&self) -> Option<Arc<LibrarySnapshot>> {
        self.library_snapshot
            .as_ref()
            .map(|cell| cell.read().unwrap_or_else(|err| err.into_inner()).clone())
    }

    pub(crate) fn busy_drive_bays(&self) -> std::collections::HashSet<u16> {
        self.drive_pool
            .as_ref()
            .map(crate::write_owner::DrivePool::busy_bays)
            .unwrap_or_default()
    }

    #[allow(dead_code)]
    pub(crate) fn drive_uuid_for_bay(&self, bay: u16) -> Result<Option<Vec<u8>>, Status> {
        let library_serial = if let Some(serial) = self.default_library_serial.as_deref() {
            serial.to_string()
        } else if let Some(snapshot) = self.current_library_snapshot() {
            match snapshot.report.libraries.as_slice() {
                [library] => library.serial.clone(),
                _ => return Ok(None),
            }
        } else {
            let index = self.index()?;
            let mut serials = index
                .list_drives(true, false)
                .map_err(status_from_state_error)?
                .into_iter()
                .filter_map(|drive| {
                    drive
                        .last_library_serial
                        .map(|serial| serial.trim().to_string())
                        .filter(|serial| !serial.is_empty())
                })
                .collect::<std::collections::HashSet<_>>();
            match serials.len() {
                1 => serials
                    .drain()
                    .next()
                    .expect("single drive library serial must exist"),
                _ => return Ok(None),
            }
        };
        let index = self.index()?;
        let drive = index
            .get_actionable_drive_at(library_serial.as_str(), i64::from(bay))
            .map_err(status_from_state_error)?;
        Ok(drive.map(|drive| drive.drive_uuid))
    }

    fn drive_record_at_bay(
        &self,
        library_serial: &str,
        bay: u16,
    ) -> Result<Option<remanence_state::DriveRecord>, Status> {
        let index = self.index()?;
        let drive = index
            .list_drives(true, false)
            .map_err(status_from_state_error)?
            .into_iter()
            .find(|drive| {
                drive.last_library_serial.as_deref() == Some(library_serial)
                    && drive.last_element_address == Some(i64::from(bay))
            });
        Ok(drive)
    }

    fn library_is_managed(&self, library_serial: &str) -> bool {
        self.managed_library_serials.is_empty()
            || self.managed_library_serials.contains(library_serial.trim())
    }

    fn drive_counters(&self, drive_uuid: &[u8]) -> Arc<DriveByteCounters> {
        self.live_status
            .get_or_create_counters(self.daemon_epoch, drive_uuid)
    }

    fn record_drive_bytes(&self, drive_uuid: Option<&[u8]>, bytes: u64, kind: &'static str) {
        let Some(drive_uuid) = drive_uuid.filter(|drive_uuid| !drive_uuid.is_empty()) else {
            tracing::warn!(kind, bytes, "skipping byte accounting for unresolved drive");
            return;
        };
        let counters = self.drive_counters(drive_uuid);
        match kind {
            "read" => {
                counters.read_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            "write" => {
                counters.write_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            _ => unreachable!("byte-accounting kind must be read or write"),
        }
    }

    pub(crate) fn record_drive_read_bytes(&self, drive_uuid: Option<&[u8]>, bytes: u64) {
        self.record_drive_bytes(drive_uuid, bytes, "read");
    }

    pub(crate) fn record_drive_write_bytes(&self, drive_uuid: Option<&[u8]>, bytes: u64) {
        self.record_drive_bytes(drive_uuid, bytes, "write");
    }

    async fn live_status_response(&self) -> Result<pb::GetLiveStatusResponse, Status> {
        let snapshot_at = OffsetDateTime::now_utc();
        if let Some(cached) = self
            .live_status
            .cache
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
        {
            if snapshot_at - cached.0 < self.live_status.min_poll_interval {
                return Ok(cached.1);
            }
        }

        let snapshot = self
            .current_library_snapshot()
            .ok_or_else(|| Status::not_found("library not found"))?;
        let index = self.index()?;
        let voltags = crate::library::voltag_uuid_map(&index)?;
        let busy_bays = self.busy_drive_bays();
        let active_clean_run_drive_uuids = index
            .list_clean_runs(false)
            .map_err(status_from_state_error)?
            .into_iter()
            .filter(|run| !matches!(run.phase.as_str(), "done" | "failed" | "needs-operator"))
            .map(|run| run.drive_uuid)
            .collect::<HashSet<_>>();
        let open_session_by_drive = index
            .non_terminal_sessions()
            .map_err(status_from_state_error)?
            .into_iter()
            .filter_map(|session| {
                let drive_uuid = session.drive_uuid?;
                Some((drive_uuid, session.session_id.as_bytes().to_vec()))
            })
            .collect::<HashMap<Vec<u8>, Vec<u8>>>();

        let mut libraries = Vec::new();
        for library in &snapshot.report.libraries {
            let mut state = crate::library::project_library_state(
                library,
                &snapshot.captured_at,
                &voltags,
                &busy_bays,
                &HashSet::new(),
            );
            state.managed = if self.library_is_managed(library.serial.as_str()) {
                "rem".to_string()
            } else {
                "foreign".to_string()
            };

            for drive in state.drives.iter_mut() {
                let bay = u16::try_from(drive.element_address)
                    .map_err(|_| Status::invalid_argument("drive element address overflows u16"))?;
                let record = self.drive_record_at_bay(library.serial.as_str(), bay)?;
                if let Some(record) = record {
                    self.enrich_live_drive(
                        drive,
                        &record,
                        active_clean_run_drive_uuids.contains(&record.drive_uuid),
                        open_session_by_drive.get(&record.drive_uuid),
                    );
                }
            }
            libraries.push(state);
        }

        let operations = index
            .list_operations()
            .map_err(status_from_state_error)?
            .into_iter()
            .filter_map(|operation| {
                Uuid::parse_str(operation.operation_id.as_str())
                    .ok()
                    .map(|operation_id| pb::OperationRef {
                        operation_id: operation_id.as_bytes().to_vec(),
                    })
            })
            .collect::<Vec<_>>();
        let alarms = index
            .list_alarms(false)
            .map_err(status_from_state_error)?
            .into_iter()
            .map(alarm_record_to_proto)
            .collect::<Vec<_>>();
        let snapshot_at_utc = snapshot_at
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
        let response = pb::GetLiveStatusResponse {
            libraries,
            operations,
            alarms,
            snapshot_at_utc,
            daemon_epoch: self.daemon_epoch,
        };
        *self
            .live_status
            .cache
            .write()
            .unwrap_or_else(|err| err.into_inner()) = Some((snapshot_at, response.clone()));
        Ok(response)
    }

    fn enrich_live_drive(
        &self,
        drive: &mut pb::Drive,
        record: &remanence_state::DriveRecord,
        cleaning_active: bool,
        open_session_id: Option<&Vec<u8>>,
    ) {
        let drive_uuid = record.drive_uuid.clone();
        drive.drive_uuid = drive_uuid.clone();
        drive.cleaning_due = if record.managed == "foreign" {
            "none".to_string()
        } else {
            record.cleaning_due.clone()
        };
        drive.fenced = record.fenced;
        drive.lifetime_read_bytes = self
            .live_status
            .drive_counters
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .get(&drive_uuid)
            .map(|counters| counters.read_bytes.load(Ordering::Relaxed))
            .unwrap_or(0);
        drive.lifetime_write_bytes = self
            .live_status
            .drive_counters
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .get(&drive_uuid)
            .map(|counters| counters.write_bytes.load(Ordering::Relaxed))
            .unwrap_or(0);
        drive.counter_epoch = self
            .live_status
            .drive_counters
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .get(&drive_uuid)
            .map(|counters| counters.counter_epoch)
            .unwrap_or_else(|| {
                LiveStatusState::counter_epoch(self.daemon_epoch, drive_uuid.as_slice())
            });
        drive.session_id = open_session_id.cloned().unwrap_or_default();
        drive.active_alert_names = if cleaning_active {
            vec!["cleaning".to_string()]
        } else {
            Vec::new()
        };
        if cleaning_active {
            drive.status = pb::drive::Status::DriveStatusCleaning as i32;
        } else if drive.fenced || record.fenced {
            drive.status = pb::drive::Status::DriveStatusFenced as i32;
        }
    }

    fn index_path(&self) -> PathBuf {
        self.index_path.as_ref().clone()
    }

    fn spool_dir(&self) -> Result<&Path, Status> {
        self.spool_dir
            .as_deref()
            .map(PathBuf::as_path)
            .ok_or_else(|| Status::unavailable("daemon has no write spool (read-only mode)"))
    }

    async fn acquire_spool_budget(&self, cap_bytes: u64) -> Result<OwnedSemaphorePermit, Status> {
        let permits = spool_budget_permits(cap_bytes);
        let budget = self
            .spool_budget
            .as_ref()
            .cloned()
            .ok_or_else(|| Status::unavailable("daemon has no write spool (read-only mode)"))?;
        budget
            .acquire_many_owned(permits)
            .await
            .map_err(|_| Status::unavailable("append spool budget is closed"))
    }

    pub(crate) fn drive_pool(&self) -> Result<&crate::write_owner::DrivePool, Status> {
        self.drive_pool
            .as_ref()
            .ok_or_else(|| Status::unavailable("daemon has no drive pool (read-only mode)"))
    }

    pub(crate) fn pool_config(&self, pool_id: &str) -> Result<TapePoolConfig, Status> {
        let pool_id = pool_id.trim();
        self.pool_configs
            .get(pool_id)
            .cloned()
            .ok_or_else(|| Status::invalid_argument(format!("unknown tape pool {pool_id}")))
    }

    fn reconcile_drive_catalog_from_report(
        &self,
        config: &RemConfig,
        report: &remanence_library::DiscoveryReport,
    ) -> Result<(), Status> {
        let mut index = self.index_write()?;
        observe_drive_catalog_from_libraries(
            &mut index,
            report.libraries.iter(),
            &drive_managed_library_serials(config),
        )
    }

    fn reconcile_clean_runs_from_report(
        &self,
        report: &remanence_library::DiscoveryReport,
    ) -> Result<(), Status> {
        let mut index = self.index_write()?;
        let mut reconciled = 0u64;
        for library in &report.libraries {
            reconciled = reconciled.saturating_add(
                index
                    .reconcile_clean_runs_against_library(library)
                    .map_err(status_from_state_error)?,
            );
        }
        if reconciled > 0 {
            tracing::info!("reconciled {reconciled} clean run(s) during startup");
        }
        Ok(())
    }

    fn record_request_received(
        &self,
        actor: AuditActor,
        operation_id: Uuid,
        operation_kind: &str,
        tape_uuid: &[u8; 16],
        idempotency_key: Option<Uuid>,
    ) -> Result<(), Status> {
        let mut index = CatalogIndex::open(self.index_path.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?;
        let mut detail = BTreeMap::new();
        detail.insert(
            "tape_uuid".to_string(),
            CborValue::Bytes(tape_uuid.to_vec()),
        );
        append_operation_audit(
            &mut index,
            self.audit_dir.as_ref(),
            self.audit_fsync,
            &self.audit_append_lock,
            OperationAuditInput {
                actor,
                operation_id,
                operation_kind,
                event: AuditEvent::RequestReceived,
                subject_kind: "tape",
                subject_id: Some(Uuid::from_bytes(*tape_uuid).to_string()),
                idempotency_key,
                detail,
            },
        )
    }

    fn record_library_request_received(
        &self,
        actor: AuditActor,
        operation_id: Uuid,
        operation_kind: &str,
        library_serial: &str,
        mut detail: BTreeMap<String, CborValue>,
    ) -> Result<(), Status> {
        let mut index = CatalogIndex::open(self.index_path.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?;
        detail.insert(
            "library_serial".to_string(),
            CborValue::Text(library_serial.to_string()),
        );
        append_operation_audit(
            &mut index,
            self.audit_dir.as_ref(),
            self.audit_fsync,
            &self.audit_append_lock,
            OperationAuditInput {
                actor,
                operation_id,
                operation_kind,
                event: AuditEvent::RequestReceived,
                subject_kind: "library",
                subject_id: Some(library_serial.to_string()),
                idempotency_key: None,
                detail,
            },
        )
    }

    fn record_cancel_requested(
        &self,
        actor: AuditActor,
        operation_id: Uuid,
        idempotency_key: Option<Uuid>,
        force: bool,
    ) -> Result<(), Status> {
        let mut index = CatalogIndex::open(self.index_path.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?;
        let mut detail = BTreeMap::new();
        detail.insert("force".to_string(), CborValue::Bool(force));
        append_operation_audit(
            &mut index,
            self.audit_dir.as_ref(),
            self.audit_fsync,
            &self.audit_append_lock,
            OperationAuditInput {
                actor,
                operation_id,
                operation_kind: "unknown",
                event: AuditEvent::CancelRequested,
                subject_kind: "operation",
                subject_id: Some(operation_id.to_string()),
                idempotency_key,
                detail,
            },
        )
    }

    fn record_operation_failed(
        &self,
        operation_id: Uuid,
        operation_kind: &str,
        error_summary: &str,
    ) -> Result<(), Status> {
        let mut index = CatalogIndex::open(self.index_path.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?;
        let mut detail = BTreeMap::new();
        detail.insert(
            "error_summary".to_string(),
            CborValue::Text(error_summary.to_string()),
        );
        append_operation_audit(
            &mut index,
            self.audit_dir.as_ref(),
            self.audit_fsync,
            &self.audit_append_lock,
            OperationAuditInput {
                actor: AuditActor::System,
                operation_id,
                operation_kind,
                event: AuditEvent::OperationFailed,
                subject_kind: "operation",
                subject_id: Some(operation_id.to_string()),
                idempotency_key: None,
                detail,
            },
        )
    }

    pub(crate) fn record_alarm_acked(
        &self,
        actor: AuditActor,
        condition_key: &str,
    ) -> Result<(), Status> {
        let mut index = CatalogIndex::open(self.index_path.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?;
        append_operation_audit(
            &mut index,
            self.audit_dir.as_ref(),
            self.audit_fsync,
            &self.audit_append_lock,
            OperationAuditInput {
                actor,
                operation_id: Uuid::new_v4(),
                operation_kind: "ack_alarm",
                event: AuditEvent::AlarmAcked,
                subject_kind: "alarm",
                subject_id: Some(condition_key.to_string()),
                idempotency_key: None,
                detail: BTreeMap::new(),
            },
        )
    }

    pub(crate) fn record_drive_audit(
        &self,
        actor: AuditActor,
        event: AuditEvent,
        drive_uuid: &[u8],
        detail: BTreeMap<String, CborValue>,
    ) -> Result<(), Status> {
        let mut index = CatalogIndex::open(self.index_path.as_ref())
            .map_err(|err| Status::internal(err.to_string()))?;
        append_operation_audit(
            &mut index,
            self.audit_dir.as_ref(),
            self.audit_fsync,
            &self.audit_append_lock,
            OperationAuditInput {
                actor,
                operation_id: Uuid::new_v4(),
                operation_kind: "drive_stewardship",
                event,
                subject_kind: "drive",
                subject_id: Some(bytes_to_hex(drive_uuid)),
                idempotency_key: None,
                detail,
            },
        )
    }
}

fn spawn_drive_collection_workers(
    index_path: PathBuf,
    report: remanence_library::DiscoveryReport,
    config: &RemConfig,
    drive_pool: crate::write_owner::DrivePool,
) {
    let heartbeat = parse_duration_or(&config.drives.heartbeat, Duration::from_secs(60 * 60));
    let heartbeat_index_path = index_path.clone();
    let heartbeat_pool = drive_pool.clone();
    std::thread::Builder::new()
        .name("rem-drive-heartbeat".to_string())
        .spawn(move || loop {
            std::thread::sleep(heartbeat);
            if let Err(err) = touch_managed_drive_heartbeats(&heartbeat_index_path, &heartbeat_pool)
            {
                tracing::warn!("managed drive heartbeat failed: {err}");
            }
        })
        .expect("spawn managed drive heartbeat worker");

    let foreign_poll = parse_duration_or(
        &config.drives.foreign_counter_poll,
        Duration::from_secs(60 * 60),
    );
    let drives_cfg = config.drives.clone();
    let daemon_libraries = config
        .libraries
        .iter()
        .map(|library| library.serial.trim().to_string())
        .filter(|serial| !serial.is_empty())
        .collect::<std::collections::HashSet<_>>();
    std::thread::Builder::new()
        .name("rem-foreign-drive-poll".to_string())
        .spawn(move || {
            foreign_drive_poll_loop(
                index_path,
                report,
                drives_cfg,
                daemon_libraries,
                foreign_poll,
            )
        })
        .expect("spawn foreign drive poll worker");
}

fn touch_managed_drive_heartbeats(
    index_path: &Path,
    drive_pool: &crate::write_owner::DrivePool,
) -> Result<(), StateError> {
    let index = CatalogIndex::open(index_path)?;
    for drive in index.list_drives(false, false)? {
        if drive.managed == "rem" && drive.state == "active" {
            let Some(bay) = drive
                .last_element_address
                .and_then(|address| u16::try_from(address).ok())
            else {
                continue;
            };
            if let Err(err) = drive_pool.heartbeat_drive(bay, drive.drive_uuid.clone()) {
                tracing::warn!(
                    "managed drive heartbeat skipped for {}: {err}",
                    drive.serial
                );
            }
        }
    }
    Ok(())
}

fn foreign_drive_poll_loop(
    index_path: PathBuf,
    report: remanence_library::DiscoveryReport,
    drives_cfg: remanence_state::DrivesConfig,
    daemon_libraries: std::collections::HashSet<String>,
    base_cadence: Duration,
) {
    let mut backoff = Duration::from_secs(0);
    loop {
        let delay = if backoff.is_zero() {
            base_cadence
        } else {
            backoff
        };
        std::thread::sleep(delay);
        match poll_foreign_drive_counters_once(&index_path, &report, &drives_cfg, &daemon_libraries)
        {
            Ok(()) => backoff = Duration::from_secs(0),
            Err(ForeignPollError::Retryable(message)) => {
                tracing::warn!("foreign drive counter poll retryable failure: {message}");
                backoff = next_backoff(backoff, base_cadence);
            }
            Err(ForeignPollError::Permanent(message)) => {
                tracing::warn!("foreign drive counter poll failed: {message}");
                backoff = Duration::from_secs(0);
            }
        }
    }
}

fn poll_foreign_drive_counters_once(
    index_path: &Path,
    report: &remanence_library::DiscoveryReport,
    drives_cfg: &remanence_state::DrivesConfig,
    daemon_libraries: &std::collections::HashSet<String>,
) -> Result<(), ForeignPollError> {
    poll_foreign_drive_counters_once_with_reader(
        index_path,
        report,
        drives_cfg,
        daemon_libraries,
        read_foreign_drive_snapshot,
    )
}

fn poll_foreign_drive_counters_once_with_reader(
    index_path: &Path,
    report: &remanence_library::DiscoveryReport,
    drives_cfg: &remanence_state::DrivesConfig,
    daemon_libraries: &std::collections::HashSet<String>,
    mut read_snapshot: impl FnMut(&Path, bool) -> Result<ForeignDriveSnapshot, ForeignPollError>,
) -> Result<(), ForeignPollError> {
    let mut index = CatalogIndex::open(index_path)
        .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
    for library in &report.libraries {
        if library_is_managed(library.serial.as_str(), drives_cfg, daemon_libraries) {
            continue;
        }
        for bay in &library.drive_bays {
            let Some(installed) = bay.installed.as_ref() else {
                continue;
            };
            let Some(sg_path) = installed.sg_path.as_ref() else {
                continue;
            };
            let installed_serial = installed.serial.trim();
            if installed_serial.is_empty() {
                continue;
            }
            let Some(drive) = index
                .get_actionable_drive_at(library.serial.as_str(), i64::from(bay.element_address))
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?
            else {
                tracing::warn!(
                    "skipping foreign drive counter attribution for unresolved or ambiguous bay library_serial={} element_address={} serial={}",
                    library.serial,
                    bay.element_address,
                    installed_serial
                );
                continue;
            };
            if drive.serial.as_str() != installed_serial {
                tracing::warn!(
                    "skipping foreign drive counter attribution for bay serial mismatch library_serial={} element_address={} observed_serial={} catalog_serial={}",
                    library.serial,
                    bay.element_address,
                    installed_serial,
                    drive.serial
                );
                continue;
            }
            if drive.managed != "foreign" || drive.state != "active" {
                continue;
            }
            let snapshot = read_snapshot(sg_path, drives_cfg.foreign_tapealert)?;
            let tape_alert_flags = snapshot.tape_alert_flags.clone();
            index
                .record_drive_health_snapshot(DriveHealthSnapshotInput {
                    drive_uuid: drive.drive_uuid.clone(),
                    trigger: "foreign-counter".to_string(),
                    session_id: None,
                    tape_alert_flags,
                    write_errors_corrected: snapshot.write_errors_corrected.and_then(u64_to_i64),
                    write_errors_uncorrected: snapshot
                        .write_errors_uncorrected
                        .and_then(u64_to_i64),
                    read_errors_corrected: snapshot.read_errors_corrected.and_then(u64_to_i64),
                    read_errors_uncorrected: snapshot.read_errors_uncorrected.and_then(u64_to_i64),
                    raw_pages: Some(
                        "{\"write_error_counter\":true,\"read_error_counter\":true}".to_string(),
                    ),
                    at_utc: None,
                })
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
            index
                .observe_foreign_drive_tapealert_advisory(
                    &drive.drive_uuid,
                    snapshot.tape_alert_flags.as_deref(),
                )
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
            index
                .touch_drive_last_seen(&drive.drive_uuid)
                .map_err(|err| ForeignPollError::Permanent(err.to_string()))?;
        }
    }
    Ok(())
}

fn library_is_managed(
    serial: &str,
    drives_cfg: &remanence_state::DrivesConfig,
    daemon_libraries: &std::collections::HashSet<String>,
) -> bool {
    let configured = drives_cfg
        .managed_libraries
        .iter()
        .map(|serial| serial.trim())
        .filter(|serial| !serial.is_empty())
        .collect::<std::collections::HashSet<_>>();
    if configured.is_empty() {
        daemon_libraries.contains(serial)
    } else {
        configured.contains(serial)
    }
}

#[derive(Debug)]
enum ForeignPollError {
    Retryable(String),
    Permanent(String),
}

struct ForeignDriveSnapshot {
    tape_alert_flags: Option<String>,
    write_errors_corrected: Option<u64>,
    write_errors_uncorrected: Option<u64>,
    read_errors_corrected: Option<u64>,
    read_errors_uncorrected: Option<u64>,
}

#[cfg(target_os = "linux")]
fn read_foreign_drive_snapshot(
    sg_path: &Path,
    foreign_tapealert: bool,
) -> Result<ForeignDriveSnapshot, ForeignPollError> {
    let inner = remanence_library::LinuxSgTransport::open(sg_path)
        .map_err(|err| ForeignPollError::Permanent(format!("open {}: {err}", sg_path.display())))?;
    let mut transport =
        remanence_library::ForeignDriveTransport::with_tapealert(inner, foreign_tapealert);
    let write = read_error_counter_page_from_transport(
        &mut transport,
        remanence_library::drive_log_sense::PAGE_WRITE_ERROR_COUNTER,
        remanence_library::drive_log_sense::build_write_error_counter_cdb,
    )?;
    let read = read_error_counter_page_from_transport(
        &mut transport,
        remanence_library::drive_log_sense::PAGE_READ_ERROR_COUNTER,
        remanence_library::drive_log_sense::build_read_error_counter_cdb,
    )?;
    let tape_alert_flags = if foreign_tapealert {
        Some(read_tape_alert_flags_from_transport(&mut transport)?)
    } else {
        None
    };
    Ok(ForeignDriveSnapshot {
        tape_alert_flags,
        write_errors_corrected: write.errors_corrected,
        write_errors_uncorrected: write.errors_uncorrected,
        read_errors_corrected: read.errors_corrected,
        read_errors_uncorrected: read.errors_uncorrected,
    })
}

#[cfg(not(target_os = "linux"))]
fn read_foreign_drive_snapshot(
    _sg_path: &Path,
    _foreign_tapealert: bool,
) -> Result<ForeignDriveSnapshot, ForeignPollError> {
    Err(ForeignPollError::Permanent(
        "foreign drive polling requires Linux SG_IO".to_string(),
    ))
}

#[cfg(target_os = "linux")]
fn read_error_counter_page_from_transport<T: remanence_library::SgTransport>(
    transport: &mut T,
    page_code: u8,
    build_cdb: fn(u16) -> [u8; 10],
) -> Result<remanence_library::drive_log_sense::ErrorCounterPage, ForeignPollError> {
    let cdb = build_cdb(remanence_library::drive_log_sense::ERROR_COUNTER_RESPONSE_LEN);
    let mut buf = [0u8; remanence_library::drive_log_sense::ERROR_COUNTER_RESPONSE_LEN as usize];
    transport.set_timeout_for(remanence_library::TimeoutClass::TapeStatus);
    let outcome = transport
        .execute_in(&cdb, &mut buf)
        .map_err(foreign_poll_error_from_scsi)?;
    let bytes = (outcome.bytes_transferred as usize).min(buf.len());
    remanence_library::drive_log_sense::parse_error_counter_response(&buf[..bytes], page_code)
        .map_err(foreign_poll_error_from_scsi)
}

#[cfg(target_os = "linux")]
fn read_tape_alert_flags_from_transport<T: remanence_library::SgTransport>(
    transport: &mut T,
) -> Result<String, ForeignPollError> {
    let cdb = remanence_library::drive_log_sense::build_tape_alert_cdb(
        remanence_library::drive_log_sense::TAPE_ALERT_RESPONSE_LEN,
    );
    let mut buf = [0u8; remanence_library::drive_log_sense::TAPE_ALERT_RESPONSE_LEN as usize];
    transport.set_timeout_for(remanence_library::TimeoutClass::TapeStatus);
    let outcome = transport
        .execute_in(&cdb, &mut buf)
        .map_err(foreign_poll_error_from_scsi)?;
    let bytes = (outcome.bytes_transferred as usize).min(buf.len());
    let alerts = remanence_library::drive_log_sense::parse_response(&buf[..bytes])
        .map_err(foreign_poll_error_from_scsi)?;
    Ok(tape_alert_flags_json(alerts.active()))
}

#[cfg(target_os = "linux")]
fn foreign_poll_error_from_scsi(err: remanence_library::ScsiError) -> ForeignPollError {
    if is_retryable_foreign_scsi_error(&err) {
        ForeignPollError::Retryable(err.to_string())
    } else {
        ForeignPollError::Permanent(err.to_string())
    }
}

#[cfg(target_os = "linux")]
fn is_retryable_foreign_scsi_error(err: &remanence_library::ScsiError) -> bool {
    match err {
        remanence_library::ScsiError::UnexpectedStatus { status } => {
            matches!(*status, 0x08 | 0x18)
        }
        remanence_library::ScsiError::CheckCondition { sense, .. }
        | remanence_library::ScsiError::TransportError { sense, .. } => {
            remanence_scsi_unit_attention(sense)
        }
        _ => false,
    }
}

#[cfg(target_os = "linux")]
fn remanence_scsi_unit_attention(sense: &[u8]) -> bool {
    remanence_library::decode_scsi_sense(sense).is_some_and(|sense| sense.key == 0x06)
}

fn next_backoff(current: Duration, max: Duration) -> Duration {
    let next = if current.is_zero() {
        Duration::from_secs(5)
    } else {
        current.saturating_mul(2)
    };
    next.min(max)
}

fn parse_duration_or(value: &str, default: Duration) -> Duration {
    parse_simple_duration(value).unwrap_or(default)
}

fn parse_simple_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let split = value.find(|ch: char| !ch.is_ascii_digit())?;
    let (digits, unit) = value.split_at(split);
    let count = digits.parse::<u64>().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(count)),
        "s" => Some(Duration::from_secs(count)),
        "m" => Some(Duration::from_secs(count.saturating_mul(60))),
        "h" => Some(Duration::from_secs(count.saturating_mul(60 * 60))),
        _ => None,
    }
}

fn tape_alert_flags_json(flags: &std::collections::BTreeSet<u8>) -> String {
    let body = flags
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{body}]")
}

fn u64_to_i64(value: u64) -> Option<i64> {
    i64::try_from(value).ok()
}

pub(crate) struct OperationAuditInput<'a> {
    pub(crate) actor: AuditActor,
    pub(crate) operation_id: Uuid,
    pub(crate) operation_kind: &'a str,
    pub(crate) event: AuditEvent,
    pub(crate) subject_kind: &'a str,
    pub(crate) subject_id: Option<String>,
    pub(crate) idempotency_key: Option<Uuid>,
    pub(crate) detail: BTreeMap<String, CborValue>,
}

pub(crate) fn append_operation_audit(
    index: &mut CatalogIndex,
    audit_dir: &Path,
    audit_fsync: bool,
    audit_append_lock: &Arc<std::sync::Mutex<()>>,
    input: OperationAuditInput<'_>,
) -> Result<(), Status> {
    let _guard = audit_append_lock
        .lock()
        .map_err(|_| Status::internal("operation audit append lock poisoned"))?;
    fs::create_dir_all(audit_dir).map_err(|err| {
        Status::internal(format!(
            "create operation audit directory {}: {err}",
            audit_dir.display()
        ))
    })?;
    let mut detail = input.detail;
    detail
        .entry("operation_kind".to_string())
        .or_insert_with(|| CborValue::Text(input.operation_kind.to_string()));
    let mut audit = FileAuditLog::open(audit_dir, audit_fsync)
        .map_err(|err| Status::internal(err.to_string()))?;
    let (_, record) = audit
        .append_and_return_record(AuditEventRecord {
            actor: input.actor,
            source_layer: SourceLayer::Layer5,
            operation_id: Some(input.operation_id),
            session_id: None,
            idempotency_key: input.idempotency_key,
            event: input.event,
            subject: AuditSubject {
                kind: input.subject_kind.to_string(),
                id: input.subject_id,
            },
            detail,
        })
        .map_err(status_from_state_error)?;
    index
        .project_audit_record(&record)
        .map_err(status_from_state_error)?;
    Ok(())
}

fn default_audit_dir_for_index(index_path: &Path) -> PathBuf {
    let Some(parent) = index_path.parent() else {
        return PathBuf::from("audit");
    };
    if parent.file_name().and_then(|name| name.to_str()) == Some("index") {
        return parent
            .parent()
            .map(|state_dir| state_dir.join("audit"))
            .unwrap_or_else(|| parent.join("audit"));
    }
    parent.join("audit")
}

fn live_status_config_from(config: &remanence_state::LiveStatusConfig) -> Duration {
    parse_simple_duration(config.min_poll_interval.as_str())
        .unwrap_or_else(|| Duration::from_millis(250))
}

/// Implementation of the process-level Daemon service.
#[derive(Clone)]
pub struct DaemonService {
    state: ApiState,
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
        Ok(Response::new(pb::HealthResponse {
            status: status as i32,
            components,
            detail: format!("sqlite quick_check={quick_check}"),
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
    state: ApiState,
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
        pool.reserve_all_exclusive()?;
        let operation_id = Uuid::new_v4();
        if let Err(err) = self.state.record_request_received(
            actor,
            operation_id,
            "reconcile_tape",
            &tape_uuid,
            None,
        ) {
            pool.release_all();
            return Err(err);
        }
        let handle = self
            .state
            .operations
            .register(operation_id, "reconcile_tape");
        match pool
            .changer_tx()
            .try_send(crate::write_owner::ChangerCommand::Reconcile {
                tape_uuid,
                handle: handle.clone(),
            }) {
            Ok(()) => Ok(Response::new(pb::OperationRef {
                operation_id: operation_id.as_bytes().to_vec(),
            })),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let error = "drive-session owner is busy";
                pool.release_all();
                self.state
                    .record_operation_failed(operation_id, "reconcile_tape", error)?;
                handle.publish_failed(error, &[("phase", "dispatch")]);
                Err(Status::failed_precondition(error))
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                let error = "drive-session owner is stopped";
                pool.release_all();
                self.state
                    .record_operation_failed(operation_id, "reconcile_tape", error)?;
                handle.publish_failed(error, &[("phase", "dispatch")]);
                Err(Status::unavailable(error))
            }
        }
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
        blocking_status(move || list_entries_for_unit(unit)).await
    }
}

/// Implementation of the Layer 5 write-session service.
#[derive(Clone)]
pub struct WriteSessionApi {
    state: ApiState,
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
        if !request.recover_session_id.is_empty() {
            return Err(Status::unimplemented(
                "recover_session_id is not wired in this write-session slice",
            ));
        }
        let body_format = if request.body_format.trim().is_empty() {
            "rao-v1".to_string()
        } else {
            request.body_format.trim().to_string()
        };
        if body_format != "rao-v1" {
            return Err(Status::unimplemented(format!(
                "write body format {body_format} is not wired in this slice"
            )));
        }
        let target = match request
            .target
            .ok_or_else(|| Status::invalid_argument("missing write-session target"))?
        {
            pb::open_write_session_request::Target::PoolTarget(target) => target,
            pb::open_write_session_request::Target::DriveTarget(_)
            | pb::open_write_session_request::Target::TapeTarget(_) => {
                return Err(Status::unimplemented(
                    "only pool-target write sessions are wired in this slice",
                ));
            }
        };
        if target.pool_id.trim().is_empty() {
            return Err(Status::invalid_argument("pool_id must not be empty"));
        }
        if !target.mount_if_needed {
            return Err(Status::invalid_argument(
                "pool-target write sessions require mount_if_needed=true in this slice",
            ));
        }
        let library_serial = self.library_serial_for_pool_target(&target)?;
        let session =
            crate::mount::open_write_session(&self.state, target.pool_id, library_serial).await?;
        Ok(Response::new(session))
    }

    async fn append_object(
        &self,
        request: Request<tonic::Streaming<pb::AppendObjectMessage>>,
    ) -> Result<Response<pb::ObjectRecord>, Status> {
        authorize_request(&request, AuthPermission::Write)?;
        let mut stream = request.into_inner();
        let first = stream
            .message()
            .await
            .map_err(|err| Status::invalid_argument(format!("append stream failed: {err}")))?
            .ok_or_else(|| Status::invalid_argument("append stream is empty"))?;
        let start = match first.payload {
            Some(pb::append_object_message::Payload::Start(start)) => start,
            _ => {
                return Err(Status::invalid_argument(
                    "first AppendObject message must be Start",
                ));
            }
        };
        if !start.body_format_manifest.is_empty() {
            return Err(Status::unimplemented(
                "body_format_manifest is not wired in this write-session slice",
            ));
        }
        let session_id = decode_uuid_bytes(&start.session_id, "session_id")?;
        let session_id = Uuid::from_bytes(session_id);
        let cap = append_spool_cap(start.declared_size_bytes);
        let _spool_budget = self.state.acquire_spool_budget(cap).await?;
        let mut spool = create_append_spool(self.state.spool_dir()?.to_path_buf(), cap).await?;
        let mut finish = None;
        let spool_started = Instant::now();
        let mut spool_bytes = 0u64;
        let mut spool_chunks = 0u64;
        while let Some(message) = stream
            .message()
            .await
            .map_err(|err| Status::invalid_argument(format!("append stream failed: {err}")))?
        {
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
                    spool = write_append_spool_chunk(spool, chunk.data).await?;
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
            }
        }
        let finish =
            finish.ok_or_else(|| Status::invalid_argument("append stream must end with Finish"))?;
        let expected_content_sha256 = expected_content_sha256(&finish.expected_content_sha256)?;
        let archive_path = archive_path_from_start(&start);
        let spool_path = finish_append_spool(spool).await?;
        let spool_elapsed = spool_started.elapsed();
        tracing::info!(
            target: "remanence_write_diag",
            "remanence_write_diag phase=spool session_id={} caller_object_id={:?} payload_bytes={} chunks={} declared_size_bytes={} elapsed_ms={:.3} throughput_mib_s={:.3}",
            session_id,
            start.caller_object_id,
            spool_bytes,
            spool_chunks,
            start.declared_size_bytes,
            crate::diagnostics::duration_ms(spool_elapsed),
            crate::diagnostics::mib_per_s(spool_bytes, spool_elapsed),
        );
        let caller_object_id = start.caller_object_id;
        let caller_object_id_for_diag = caller_object_id.clone();
        let append_finish_started = Instant::now();
        let record = match crate::mount::append_finish(
            &self.state,
            session_id,
            spool_path.clone(),
            archive_path,
            caller_object_id,
            expected_content_sha256,
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
            "remanence_write_diag phase=append_finish session_id={} caller_object_id={:?} payload_bytes={} elapsed_ms={:.3} throughput_mib_s={:.3}",
            session_id,
            caller_object_id_for_diag,
            spool_bytes,
            crate::diagnostics::duration_ms(append_finish_elapsed),
            crate::diagnostics::mib_per_s(spool_bytes, append_finish_elapsed),
        );
        Ok(Response::new(record))
    }

    async fn checkpoint_session(
        &self,
        request: Request<pb::CheckpointSessionRequest>,
    ) -> Result<Response<pb::WriteSession>, Status> {
        authorize_request(&request, AuthPermission::Write)?;
        let request = request.into_inner();
        reject_unimplemented_idempotency(request.idempotency_key.as_ref(), "CheckpointSession")?;
        Err(Status::unimplemented(
            "CheckpointSession is not wired in this write-session slice",
        ))
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
        let session = crate::mount::abort_write_session(&self.state, session_id).await?;
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
    fn library_serial_for_pool_target(
        &self,
        target: &pb::TapePoolTarget,
    ) -> Result<String, Status> {
        let serial = if target.library_uuid.is_empty() {
            self.state
                .default_library_serial
                .as_ref()
                .map(|serial| serial.as_str().to_string())
                .ok_or_else(|| {
                    Status::invalid_argument(
                        "pool target library_uuid is required when config does not name exactly one library",
                    )
                })?
        } else {
            let requested = decode_uuid_bytes(target.library_uuid.as_slice(), "library_uuid")?;
            let snapshot = self
                .state
                .current_library_snapshot()
                .ok_or_else(|| Status::not_found("library not found"))?;
            snapshot
                .report
                .libraries
                .iter()
                .find(|library| crate::library::library_uuid(&library.serial) == requested)
                .map(|library| library.serial.clone())
                .ok_or_else(|| Status::not_found("library not found"))?
        };
        let serial = serial.trim().to_string();
        if serial.is_empty() {
            Err(Status::invalid_argument("library serial must not be empty"))
        } else {
            Ok(serial)
        }
    }
}

/// Implementation of the Layer 5 read-session service.
#[derive(Clone)]
pub struct ReadSessionApi {
    state: ApiState,
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
        let tape_uuid = select_read_target(&self.state, request.target)?;
        let session = crate::mount::open_read_session(&self.state, tape_uuid).await?;
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
        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<Result<pb::BytesChunk, Status>>(16);
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
            inner: Box::pin(ReceiverStream::new(chunk_rx)),
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
        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<Result<pb::BytesChunk, Status>>(16);
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
            inner: Box::pin(ReceiverStream::new(chunk_rx)),
            state,
            drive_uuid,
        }))
    }
}

fn select_read_target(
    state: &ApiState,
    target: Option<pb::open_read_session_request::Target>,
) -> Result<[u8; 16], Status> {
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
            Ok(tape_uuid)
        }
        pb::open_read_session_request::Target::DriveTarget(_) => Err(Status::unimplemented(
            "drive-target read sessions need library inventory wiring",
        )),
    }
}

fn ensure_tape_matches_pool(
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

fn ensure_same_session(value: &[u8], expected: Uuid) -> Result<(), Status> {
    let actual = decode_uuid_bytes(value, "session_id")?;
    if Uuid::from_bytes(actual) == expected {
        Ok(())
    } else {
        Err(Status::invalid_argument(
            "append stream contains more than one session_id",
        ))
    }
}

fn expected_content_sha256(value: &[u8]) -> Result<Option<[u8; 32]>, Status> {
    if value.is_empty() {
        Ok(None)
    } else {
        value.try_into().map(Some).map_err(|_| {
            Status::invalid_argument("expected_content_sha256 must be 32 bytes when supplied")
        })
    }
}

fn archive_path_from_start(start: &pb::AppendObjectStart) -> PathBuf {
    start
        .caller_metadata
        .get("path")
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if start.caller_object_id.trim().is_empty() {
                PathBuf::from("payload.bin")
            } else {
                PathBuf::from(start.caller_object_id.clone())
            }
        })
}

fn ensure_enumerate_objects_scope_is_all(
    request: &pb::EnumerateObjectsRequest,
) -> Result<(), Status> {
    match request.scope.as_ref() {
        None | Some(pb::enumerate_objects_request::Scope::All(_)) => Ok(()),
        Some(_) => Err(Status::unimplemented(
            "scoped object enumeration is not wired in this slice",
        )),
    }
}

fn ensure_enumerate_units_scope_is_all(request: &pb::EnumerateUnitsRequest) -> Result<(), Status> {
    match request.scope.as_ref() {
        None | Some(pb::enumerate_units_request::Scope::All(_)) => Ok(()),
        Some(_) => Err(Status::unimplemented(
            "scoped catalog unit enumeration is not wired in this slice",
        )),
    }
}

fn ensure_unpaged(page_token: Option<&pb::PageToken>, page_size: u32) -> Result<(), Status> {
    if page_size != 0 || page_token.is_some_and(|token| !token.value.is_empty()) {
        return Err(Status::unimplemented(
            "paginated catalog listing is not wired in this slice",
        ));
    }
    Ok(())
}

fn append_spool_cap(declared_size_bytes: u64) -> u64 {
    if declared_size_bytes == 0 {
        crate::write_owner::SPOOL_MAX_BYTES
    } else {
        declared_size_bytes.min(crate::write_owner::SPOOL_MAX_BYTES)
    }
}

const SPOOL_BUDGET_UNIT_BYTES: u64 = 1024 * 1024;

fn spool_budget_permits(bytes: u64) -> u32 {
    let units = bytes.div_ceil(SPOOL_BUDGET_UNIT_BYTES).max(1);
    u32::try_from(units).unwrap_or(u32::MAX)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthPermission {
    Read,
    ReadTape,
    Write,
    Robotics,
    Lifecycle,
    OperationControl,
}

impl AuthPermission {
    fn label(self) -> &'static str {
        match self {
            Self::Read => "read-only RPCs",
            Self::ReadTape => "read-session RPCs",
            Self::Write => "write-session RPCs",
            Self::Robotics => "library robotics RPCs",
            Self::Lifecycle => "lifecycle RPCs",
            Self::OperationControl => "operation-control RPCs",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClientRole {
    System,
    Readonly,
    Operator,
    Orchestrator,
    Admin,
}

impl ClientRole {
    fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Readonly => "readonly",
            Self::Operator => "operator",
            Self::Orchestrator => "orchestrator",
            Self::Admin => "admin",
        }
    }

    fn allows(self, permission: AuthPermission) -> bool {
        match self {
            Self::System => true,
            Self::Admin | Self::Orchestrator => !matches!(permission, AuthPermission::Lifecycle),
            Self::Operator => !matches!(
                permission,
                AuthPermission::Write | AuthPermission::Lifecycle
            ),
            Self::Readonly => matches!(permission, AuthPermission::Read | AuthPermission::ReadTape),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthContext {
    actor: AuditActor,
    role: ClientRole,
}

pub(crate) fn authorize_request<T>(
    request: &Request<T>,
    permission: AuthPermission,
) -> Result<AuditActor, Status> {
    let auth = auth_context_from_request(request)?;
    if auth.role.allows(permission) {
        Ok(auth.actor)
    } else {
        Err(Status::permission_denied(format!(
            "role {} is not authorized for {}",
            auth.role.label(),
            permission.label()
        )))
    }
}

fn auth_context_from_request<T>(request: &Request<T>) -> Result<AuthContext, Status> {
    let actor = actor_from_request(request);
    let role = if let Some(certs) = request.peer_certs() {
        certs
            .first()
            .and_then(|cert| role_from_certificate_subject(cert.as_ref()))
            .unwrap_or(ClientRole::Readonly)
    } else {
        role_from_metadata(request)?.unwrap_or(ClientRole::System)
    };
    Ok(AuthContext { actor, role })
}

fn role_from_metadata<T>(request: &Request<T>) -> Result<Option<ClientRole>, Status> {
    let Some(value) = request.metadata().get("x-remanence-role") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| Status::permission_denied("x-remanence-role must be printable ASCII"))?;
    parse_client_role(value)
        .map(Some)
        .ok_or_else(|| Status::permission_denied("unrecognized x-remanence-role"))
}

fn role_from_certificate_subject(cert_der: &[u8]) -> Option<ClientRole> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der).ok()?;
    for attr in cert.subject().iter_attributes() {
        if let Ok(value) = attr.as_str() {
            if let Some(role) = parse_certificate_role_attribute(value) {
                return Some(role);
            }
        }
    }
    None
}

/// Certificate subjects grant a role only through an explicit
/// `remanence-role=<role>` (or `remanence-role:<role>`) attribute
/// value. Bare role words are deliberately NOT honored here: subject
/// attributes routinely carry human-chosen names, and a certificate
/// whose CN happens to read "operator" or "admin" must not silently
/// receive that privilege. (The `x-remanence-role` metadata path keeps
/// accepting bare words — there the header name itself states intent.)
fn parse_certificate_role_attribute(value: &str) -> Option<ClientRole> {
    let lower = value.trim().to_ascii_lowercase();
    let stripped = lower
        .strip_prefix("remanence-role=")
        .or_else(|| lower.strip_prefix("remanence-role:"))?;
    parse_role_word(stripped.trim())
}

fn parse_client_role(value: &str) -> Option<ClientRole> {
    let lower = value.trim().to_ascii_lowercase();
    let mut value = lower.as_str();
    for prefix in ["remanence-role=", "remanence-role:", "role=", "role:"] {
        if let Some(stripped) = value.strip_prefix(prefix) {
            value = stripped.trim();
            break;
        }
    }
    parse_role_word(value)
}

fn parse_role_word(value: &str) -> Option<ClientRole> {
    match value {
        "system" => Some(ClientRole::System),
        "readonly" | "read-only" | "read_only" => Some(ClientRole::Readonly),
        "operator" => Some(ClientRole::Operator),
        "orchestrator" => Some(ClientRole::Orchestrator),
        "admin" => Some(ClientRole::Admin),
        _ => None,
    }
}

async fn create_append_spool(dir: PathBuf, cap: u64) -> Result<crate::write_owner::Spool, Status> {
    tokio::task::spawn_blocking(move || crate::write_owner::Spool::create(&dir, cap))
        .await
        .map_err(|err| Status::internal(format!("create append spool task failed: {err}")))?
        .map_err(|err| Status::internal(format!("create append spool: {err}")))
}

async fn write_append_spool_chunk(
    spool: crate::write_owner::Spool,
    data: Vec<u8>,
) -> Result<crate::write_owner::Spool, Status> {
    tokio::task::spawn_blocking(move || {
        let mut spool = spool;
        spool.write_chunk(&data).map(|()| spool)
    })
    .await
    .map_err(|err| Status::internal(format!("write append spool task failed: {err}")))?
    .map_err(status_from_append_spool_write_error)
}

async fn finish_append_spool(spool: crate::write_owner::Spool) -> Result<PathBuf, Status> {
    tokio::task::spawn_blocking(move || spool.finish())
        .await
        .map_err(|err| Status::internal(format!("finish append spool task failed: {err}")))?
        .map_err(|err| Status::internal(format!("finish append spool: {err}")))
}

fn status_from_append_spool_write_error(err: io::Error) -> Status {
    if err.kind() == io::ErrorKind::InvalidInput {
        Status::resource_exhausted("object exceeds append spool size cap")
    } else {
        Status::internal(format!("write append spool: {err}"))
    }
}

fn identity_source_name(source: remanence_library::IdentitySource) -> &'static str {
    match source {
        remanence_library::IdentitySource::DvcidInline => "DvcidInline",
        remanence_library::IdentitySource::DvcidAndInquiry => "DvcidAndInquiry",
        remanence_library::IdentitySource::Derived => "Derived",
    }
}

pub(crate) fn drive_managed_library_serials(config: &RemConfig) -> HashSet<String> {
    let configured = config
        .drives
        .managed_libraries
        .iter()
        .map(|serial| serial.trim().to_string())
        .filter(|serial| !serial.is_empty())
        .collect::<HashSet<_>>();
    if !configured.is_empty() {
        return configured;
    }
    config
        .libraries
        .iter()
        .map(|library| library.serial.trim().to_string())
        .filter(|serial| !serial.is_empty())
        .collect()
}

pub(crate) fn observe_drive_catalog_from_libraries<'a>(
    index: &mut CatalogIndex,
    libraries: impl IntoIterator<Item = &'a remanence_library::Library>,
    managed_library_serials: &HashSet<String>,
) -> Result<(), Status> {
    let observations = libraries
        .into_iter()
        .flat_map(|library| {
            let managed = managed_library_serials.contains(library.serial.as_str());
            library.drive_bays.iter().filter_map(move |bay| {
                let installed = bay.installed.as_ref()?;
                Some(remanence_state::DriveObservationInput {
                    serial: installed.serial.clone(),
                    identity_source: identity_source_name(installed.identity_source).to_string(),
                    vendor: installed.vendor.clone(),
                    product: installed.product.clone(),
                    firmware_rev: installed.revision.clone(),
                    managed: if managed { "rem" } else { "foreign" }.to_string(),
                    library_serial: Some(library.serial.clone()),
                    element_address: Some(i64::from(bay.element_address)),
                    observed_at_utc: None,
                })
            })
        })
        .collect::<Vec<_>>();
    index
        .observe_drive_inventory_snapshot(observations)
        .map(|_| ())
        .map_err(status_from_state_error)
}

pub(crate) fn actor_from_request<T>(request: &Request<T>) -> AuditActor {
    if let Some(certs) = request.peer_certs() {
        if let Some(cert) = certs.first() {
            return AuditActor::Service(format!(
                "mtls-cert-sha256:{}",
                hex_lower(&Sha256::digest(cert.as_ref()))
            ));
        }
    }

    request
        .metadata()
        .get("x-remanence-actor")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| AuditActor::Service(value.to_string()))
        .unwrap_or(AuditActor::System)
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    hex_lower(bytes)
}

pub(crate) fn status_from_state_error(err: StateError) -> Status {
    match err {
        StateError::ConfigInvalid(_) => Status::invalid_argument(err.to_string()),
        _ => Status::internal(err.to_string()),
    }
}

fn find_object_for_key(
    state: &ApiState,
    key: Option<pb::get_object_request::Key>,
) -> Result<Option<NativeObjectRecord>, Status> {
    match key.ok_or_else(|| Status::invalid_argument("missing object lookup key"))? {
        pb::get_object_request::Key::ObjectId(value) => {
            let object_id = decode_object_id(&value)?;
            state
                .index()?
                .get_native_object(object_id.as_str())
                .map_err(|err| Status::internal(err.to_string()))
        }
        pb::get_object_request::Key::ContentSha256(hash) => state
            .index()?
            .get_native_object_by_content_hash(hash.as_slice())
            .map_err(|err| Status::internal(err.to_string())),
        pb::get_object_request::Key::CallerObjectId(caller_id) => state
            .index()?
            .get_native_object_by_caller_object_id(caller_id.as_str())
            .map_err(|err| Status::internal(err.to_string())),
    }
}

fn find_copy_object_for_key(
    state: &ApiState,
    key: Option<pb::find_object_copies_request::Key>,
) -> Result<Option<NativeObjectRecord>, Status> {
    let get_key = match key.ok_or_else(|| Status::invalid_argument("missing object lookup key"))? {
        pb::find_object_copies_request::Key::ObjectId(value) => {
            pb::get_object_request::Key::ObjectId(value)
        }
        pb::find_object_copies_request::Key::ContentSha256(value) => {
            pb::get_object_request::Key::ContentSha256(value)
        }
        pb::find_object_copies_request::Key::CallerObjectId(value) => {
            pb::get_object_request::Key::CallerObjectId(value)
        }
    };
    find_object_for_key(state, Some(get_key))
}

fn catalog_unit_filter(value: i32) -> CatalogUnitFilter {
    if value == pb::CatalogUnitOriginFilter::NativeObjects as i32 {
        CatalogUnitFilter::NativeObjects
    } else if value == pb::CatalogUnitOriginFilter::ForeignArchives as i32 {
        CatalogUnitFilter::ForeignArchives
    } else {
        CatalogUnitFilter::All
    }
}

fn native_object_stream(
    index_path: PathBuf,
) -> Pin<Box<dyn Stream<Item = Result<pb::ObjectRecord, Status>> + Send + 'static>> {
    let (tx, rx) = tokio::sync::mpsc::channel(CATALOG_STREAM_BUFFER);
    tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<(), Status> {
            let index = CatalogIndex::open_read_only(&index_path)
                .map_err(|err| Status::internal(err.to_string()))?;
            index
                .for_each_native_object(|record| {
                    let item = object_record_to_proto(record);
                    send_stream_item(&tx, item)
                })
                .map_err(|err| Status::internal(err.to_string()))
        })();
        if let Err(status) = result {
            let _ = tx.blocking_send(Err(status));
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

fn catalog_unit_stream(
    index_path: PathBuf,
    filter: CatalogUnitFilter,
) -> Pin<Box<dyn Stream<Item = Result<pb::CatalogUnit, Status>> + Send + 'static>> {
    let (tx, rx) = tokio::sync::mpsc::channel(CATALOG_STREAM_BUFFER);
    tokio::task::spawn_blocking(move || {
        let result = (|| -> Result<(), Status> {
            let index = CatalogIndex::open_read_only(&index_path)
                .map_err(|err| Status::internal(err.to_string()))?;
            index
                .for_each_catalog_unit(filter, |record| {
                    let item = catalog_unit_to_proto(record);
                    send_stream_item(&tx, item)
                })
                .map_err(|err| Status::internal(err.to_string()))
        })();
        if let Err(status) = result {
            let _ = tx.blocking_send(Err(status));
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

fn send_stream_item<T>(
    tx: &tokio::sync::mpsc::Sender<Result<T, Status>>,
    item: Result<T, Status>,
) -> ControlFlow<()> {
    let should_continue = match item {
        Ok(value) => tx.blocking_send(Ok(value)).is_ok(),
        Err(status) => {
            let _ = tx.blocking_send(Err(status));
            false
        }
    };
    if should_continue {
        ControlFlow::Continue(())
    } else {
        ControlFlow::Break(())
    }
}

async fn blocking_status<T, F>(work: F) -> Result<T, Status>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Status> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| Status::internal(format!("blocking task failed: {err}")))?
}

fn operation_to_proto(record: OperationRecord) -> Result<pb::OperationStatus, Status> {
    let operation_id = encode_uuid_text(record.operation_id.as_str())?;
    let error_summary = match record.state.as_str() {
        "failed" => "operation failed",
        "completion_unknown" => "completion unknown",
        _ => "",
    }
    .to_string();
    Ok(pb::OperationStatus {
        operation_id,
        operation_kind: record.operation_kind,
        state: operation_state(record.state.as_str()) as i32,
        created_at: timestamp_from_rfc3339(record.started_at_utc.as_str()),
        updated_at: timestamp_from_rfc3339(record.updated_at_utc.as_str()),
        progress: std::collections::HashMap::new(),
        error_summary,
    })
}

pub(crate) fn operation_state(value: &str) -> pb::OperationState {
    match value {
        "queued" => pb::OperationState::Queued,
        "running" | "cancel_requested" => pb::OperationState::Running,
        "finished" | "completed_after_cancel" => pb::OperationState::Succeeded,
        "failed" => pb::OperationState::Failed,
        "cancelled_before_dispatch" => pb::OperationState::Cancelled,
        "completion_unknown" => pb::OperationState::Unknown,
        _ => pb::OperationState::Unspecified,
    }
}

fn tape_to_proto(record: TapeRecord) -> pb::Tape {
    pb::Tape {
        tape_uuid: record.tape_uuid,
        voltag: record.voltag.unwrap_or_default(),
        body_format: record.body_format.unwrap_or_default(),
        block_size_bytes: record.block_size.unwrap_or_default(),
        data_blocks_per_stripe: record.data_blocks_per_stripe.unwrap_or_default(),
        parity_blocks_per_stripe: record.parity_blocks_per_stripe.unwrap_or_default(),
        stripes_per_neighborhood: record.stripes_per_neighborhood.unwrap_or_default(),
        last_committed_tape_file: record.last_committed_tape_file.unwrap_or_default(),
        state: tape_state(record.state.as_str()) as i32,
        updated_at: timestamp_from_rfc3339(record.updated_at_utc.as_str()),
        pool_id: record.pool_id.unwrap_or_default(),
        correlation_rollups: Vec::new(),
    }
}

fn tape_to_proto_with_rollups(
    record: TapeRecord,
    rollups: Vec<DriveCorrelationRollupRecord>,
) -> pb::Tape {
    let mut tape = tape_to_proto(record);
    tape.correlation_rollups = rollups
        .into_iter()
        .map(correlation_rollup_to_proto)
        .collect();
    tape
}

fn correlation_rollup_to_proto(record: DriveCorrelationRollupRecord) -> pb::DriveCorrelationRollup {
    pb::DriveCorrelationRollup {
        tape_uuid: record.tape_uuid.unwrap_or_default(),
        voltag: record.voltag.unwrap_or_default(),
        drive_uuid: record.drive_uuid.unwrap_or_default(),
        drive_serial: record.drive_serial.unwrap_or_default(),
        session_count: u64::try_from(record.session_count).unwrap_or_default(),
        snapshot_count: u64::try_from(record.snapshot_count).unwrap_or_default(),
        write_errors_corrected: u64::try_from(record.write_errors_corrected).unwrap_or_default(),
        write_errors_uncorrected: u64::try_from(record.write_errors_uncorrected)
            .unwrap_or_default(),
        read_errors_corrected: u64::try_from(record.read_errors_corrected).unwrap_or_default(),
        read_errors_uncorrected: u64::try_from(record.read_errors_uncorrected).unwrap_or_default(),
        first_session_utc: record
            .first_session_utc
            .as_deref()
            .and_then(timestamp_from_rfc3339),
        last_session_utc: record
            .last_session_utc
            .as_deref()
            .and_then(timestamp_from_rfc3339),
    }
}

fn tape_state(value: &str) -> pb::tape::State {
    match value {
        "ingested" => pb::tape::State::TapeStateReady,
        "ready" => pb::tape::State::TapeStateReady,
        "sealed" => pb::tape::State::TapeStateSealed,
        "ingestion_pending" => pb::tape::State::TapeStateInventoried,
        "degraded" => pb::tape::State::TapeStateDegraded,
        "failed" => pb::tape::State::TapeStateFailed,
        // `retired` intentionally maps to UNSPECIFIED until the proto enum
        // gains an explicit value; add it alongside the missing
        // ready/sealed-adjacent states when the enum is next revised.
        _ => pb::tape::State::TapeStateUnspecified,
    }
}

fn tape_file_to_proto(record: TapeFileRecord) -> Result<pb::TapeFile, Status> {
    Ok(pb::TapeFile {
        tape_uuid: record.tape_uuid,
        tape_file_number: u64::from(record.tape_file_number),
        kind: record.kind,
        block_count: record.block_count,
        object_id: record
            .object_id
            .as_deref()
            .map(encode_uuid_text)
            .transpose()?
            .unwrap_or_default(),
    })
}

fn native_object_file_to_proto(record: NativeObjectFileRecord) -> Result<pb::FileRecord, Status> {
    Ok(pb::FileRecord {
        object_id: encode_uuid_text(record.object_id.as_str())?,
        file_id: record.file_id.into_bytes(),
        path: record.path,
        size_bytes: record.size_bytes,
        file_sha256: record.file_sha256,
        first_chunk_body_lba: record.first_chunk_lba.unwrap_or_default(),
        chunk_count: u32::try_from(record.chunk_count)
            .map_err(|_| Status::internal("object file chunk_count does not fit u32"))?,
    })
}

fn tape_pool_to_proto(record: TapePoolRecord) -> pb::TapePool {
    pb::TapePool {
        pool_id: record.pool_id,
        display_name: record.display_name.unwrap_or_default(),
        copy_class: record.copy_class.unwrap_or_default(),
        content_class: record.content_class.unwrap_or_default(),
    }
}

fn object_record_to_proto(record: NativeObjectRecord) -> Result<pb::ObjectRecord, Status> {
    Ok(pb::ObjectRecord {
        object_id: encode_uuid_text(record.object_id.as_str())?,
        caller_object_id: record.caller_object_id.unwrap_or_default(),
        content_sha256: record.content_hash.unwrap_or_default(),
        logical_size_bytes: record.logical_size_bytes.unwrap_or_default(),
        body_format: record.body_format,
        caller_metadata: std::collections::HashMap::new(),
        created_at: timestamp_from_rfc3339(record.created_at_utc.as_str()),
        copies: record.copies.iter().map(object_copy_to_proto).collect(),
    })
}

fn object_copy_to_proto(copy: &NativeObjectCopyRecord) -> pb::ObjectCopy {
    let health = if copy.status == "committed" {
        pb::object_copy::Health::ObjectCopyHealthOk
    } else {
        pb::object_copy::Health::ObjectCopyHealthSuspect
    };
    pb::ObjectCopy {
        tape_uuid: copy.tape_uuid.clone(),
        tape_file_number: u64::from(copy.tape_file_number),
        first_body_lba: copy.first_body_lba,
        last_verified_at: None,
        health: health as i32,
        pool_id: copy.pool_id.clone().unwrap_or_default(),
    }
}

fn catalog_unit_to_proto(record: CatalogUnitRecord) -> Result<pb::CatalogUnit, Status> {
    let origin_kind = match record.origin_kind.as_str() {
        "native_object" => pb::CatalogUnitOriginKind::NativeObject,
        "foreign_archive" => pb::CatalogUnitOriginKind::ForeignArchive,
        other => {
            return Err(Status::internal(format!(
                "unknown catalog unit origin {other}"
            )))
        }
    };
    let origin = match origin_kind {
        pb::CatalogUnitOriginKind::NativeObject => {
            let object_id = record
                .native_object_id
                .as_deref()
                .ok_or_else(|| Status::internal("native catalog unit missing object id"))?;
            Some(pb::catalog_unit::Origin::Native(pb::NativeUnitSummary {
                object_id: encode_uuid_text(object_id)?,
            }))
        }
        pb::CatalogUnitOriginKind::ForeignArchive => Some(pb::catalog_unit::Origin::Foreign(
            pb::ForeignArchiveSummary {
                scan_id: record
                    .scan_id
                    .as_deref()
                    .map(encode_text_id)
                    .unwrap_or_default(),
                source_kind: record.source_kind.unwrap_or_default(),
                source_id: record.source_id.unwrap_or_default(),
                confidence: scan_confidence(record.confidence.as_deref()) as i32,
                last_scan_at: record
                    .last_scan_at_utc
                    .as_deref()
                    .and_then(timestamp_from_rfc3339),
                entry_count: record.entry_count.unwrap_or_default(),
                damage_event_count: record.damage_event_count.unwrap_or_default(),
            },
        )),
        pb::CatalogUnitOriginKind::Unspecified => None,
    };
    Ok(pb::CatalogUnit {
        unit_id: encode_text_id(record.unit_id.as_str()),
        tape_uuid: record.tape_uuid,
        format_id: record.format_id,
        origin_kind: origin_kind as i32,
        discovered_at: timestamp_from_rfc3339(record.created_at_utc.as_str()),
        origin,
    })
}

fn list_entries_for_unit(
    unit: CatalogUnitRecord,
) -> Result<Response<pb::ListEntriesInUnitResponse>, Status> {
    if unit.origin_kind != "foreign_archive" {
        return Err(Status::unimplemented(
            "native unit entry listing is not wired in this slice",
        ));
    }
    if unit.format_id != "remanence-bru" {
        return Err(Status::unimplemented(format!(
            "catalog entry listing for {} is not wired in this slice",
            unit.format_id
        )));
    }
    #[cfg(feature = "foreign-bru")]
    {
        let source_kind = unit
            .source_kind
            .as_deref()
            .ok_or_else(|| Status::internal("foreign catalog unit missing source_kind"))?;
        if source_kind != "byte_stream_dump" {
            return Err(Status::unimplemented(format!(
                "foreign source kind {source_kind} is not wired in this slice"
            )));
        }
        let source_id = unit
            .source_id
            .as_deref()
            .ok_or_else(|| Status::internal("foreign catalog unit missing source_id"))?;
        let file = std::fs::File::open(source_id)
            .map_err(|err| Status::internal(format!("open foreign dump source: {err}")))?;
        let mut reader = BruFormat.open_dump_reader(file);
        let mut collector = CatalogEntryCollector::new(encode_text_id(unit.unit_id.as_str()));
        reader
            .scan(&mut collector)
            .map_err(|err| Status::internal(format!("scan foreign archive: {err}")))?;
        Ok(Response::new(pb::ListEntriesInUnitResponse {
            entries: collector.entries,
            next_page_token: None,
            archive_gaps: collector.archive_gaps,
        }))
    }
    #[cfg(not(feature = "foreign-bru"))]
    {
        Err(Status::unimplemented(
            "format remanence-bru is not available in this build",
        ))
    }
}

#[cfg(feature = "foreign-bru")]
struct CatalogEntryCollector {
    unit_id: Vec<u8>,
    entries: Vec<pb::CatalogEntry>,
    archive_gaps: Vec<pb::ArchiveGap>,
    positions: std::collections::HashMap<String, usize>,
    pending_states: std::collections::HashMap<String, pb::CatalogEntryState>,
}

#[cfg(feature = "foreign-bru")]
impl CatalogEntryCollector {
    fn new(unit_id: Vec<u8>) -> Self {
        Self {
            unit_id,
            entries: Vec::new(),
            archive_gaps: Vec::new(),
            positions: std::collections::HashMap::new(),
            pending_states: std::collections::HashMap::new(),
        }
    }

    fn mark_state(&mut self, file_id: &str, state: pb::CatalogEntryState) {
        if let Some(position) = self.positions.get(file_id).copied() {
            self.entries[position].state = state as i32;
        } else {
            self.pending_states.insert(file_id.to_string(), state);
        }
    }
}

#[cfg(feature = "foreign-bru")]
impl EntryCatalogSink for CatalogEntryCollector {
    fn entry(&mut self, entry: &NormalizedEntry) -> Result<(), FormatError> {
        let file_id = entry.file_id.as_str().to_string();
        let state = self
            .pending_states
            .remove(file_id.as_str())
            .unwrap_or(pb::CatalogEntryState::Complete);
        self.positions.insert(file_id, self.entries.len());
        self.entries.push(normalized_entry_to_proto(
            self.unit_id.clone(),
            entry,
            state,
        ));
        Ok(())
    }

    fn damage(&mut self, range: &DamageRange) -> Result<(), FormatError> {
        self.mark_state(
            range.file_id.as_str(),
            catalog_entry_state_for_damage(range.status),
        );
        Ok(())
    }

    fn archive_gap(&mut self, range: &ArchiveGapRange) -> Result<(), FormatError> {
        self.archive_gaps
            .push(archive_gap_to_proto(self.unit_id.clone(), range));
        Ok(())
    }
}

#[cfg(feature = "foreign-bru")]
fn normalized_entry_to_proto(
    unit_id: Vec<u8>,
    entry: &NormalizedEntry,
    state: pb::CatalogEntryState,
) -> pb::CatalogEntry {
    pb::CatalogEntry {
        unit_id,
        entry_id: encode_text_id(entry.file_id.as_str()),
        path: entry.path.clone(),
        kind: catalog_entry_kind(entry.kind) as i32,
        size_bytes: entry.size_bytes,
        mtime: None,
        state: state as i32,
        integrity_basis: pb::IntegrityBasis::FormatChecksum as i32,
    }
}

#[cfg(feature = "foreign-bru")]
fn catalog_entry_kind(kind: EntryKind) -> pb::CatalogEntryKind {
    match kind {
        EntryKind::RegularFile => pb::CatalogEntryKind::RegularFile,
        EntryKind::Directory => pb::CatalogEntryKind::Directory,
        EntryKind::Symlink => pb::CatalogEntryKind::Symlink,
        EntryKind::Hardlink => pb::CatalogEntryKind::Hardlink,
        EntryKind::Special => pb::CatalogEntryKind::Special,
    }
}

#[cfg(feature = "foreign-bru")]
fn catalog_entry_state_for_damage(status: DamageStatus) -> pb::CatalogEntryState {
    match status {
        DamageStatus::ChecksumFailed | DamageStatus::ReadError => pb::CatalogEntryState::Damaged,
        DamageStatus::Missing => pb::CatalogEntryState::Partial,
        DamageStatus::Unsupported => pb::CatalogEntryState::Unsupported,
    }
}

#[cfg(feature = "foreign-bru")]
fn archive_gap_to_proto(unit_id: Vec<u8>, range: &ArchiveGapRange) -> pb::ArchiveGap {
    pb::ArchiveGap {
        unit_id,
        source_start: range.source_start,
        source_end: range.source_end,
        cause: archive_gap_cause(range.cause) as i32,
    }
}

#[cfg(feature = "foreign-bru")]
fn archive_gap_cause(cause: ArchiveGapCause) -> pb::ArchiveGapCause {
    match cause {
        ArchiveGapCause::UnrecognizedData => pb::ArchiveGapCause::UnrecognizedData,
        ArchiveGapCause::ReadError => pb::ArchiveGapCause::ReadError,
        ArchiveGapCause::Missing => pb::ArchiveGapCause::Missing,
        ArchiveGapCause::Resync => pb::ArchiveGapCause::Resync,
        ArchiveGapCause::Unsupported => pb::ArchiveGapCause::Unsupported,
    }
}

fn scan_confidence(value: Option<&str>) -> pb::CatalogScanConfidence {
    match value {
        Some("low") => pb::CatalogScanConfidence::Low,
        Some("medium") => pb::CatalogScanConfidence::Medium,
        Some("high") => pb::CatalogScanConfidence::High,
        _ => pb::CatalogScanConfidence::Unspecified,
    }
}

fn encode_text_id(value: &str) -> Vec<u8> {
    Uuid::parse_str(value)
        .map(|uuid| uuid.as_bytes().to_vec())
        .unwrap_or_else(|_| value.as_bytes().to_vec())
}

fn decode_object_id(value: &[u8]) -> Result<String, Status> {
    let uuid = decode_uuid_bytes(value, "object_id")?;
    Ok(Uuid::from_bytes(uuid).to_string())
}

fn decode_uuid_bytes(value: &[u8], field: &str) -> Result<[u8; 16], Status> {
    value.try_into().map_err(|_| {
        Status::invalid_argument(format!("{field} must be a 16-byte UUID byte string"))
    })
}

fn decode_optional_idempotency(value: Option<&pb::IdempotencyKey>) -> Result<Option<Uuid>, Status> {
    value
        .filter(|key| !key.value.is_empty())
        .map(|key| decode_uuid_bytes(key.value.as_slice(), "idempotency_key.value"))
        .transpose()
        .map(|uuid| uuid.map(Uuid::from_bytes))
}

pub(crate) fn reject_unimplemented_idempotency(
    value: Option<&pb::IdempotencyKey>,
    rpc: &str,
) -> Result<(), Status> {
    if decode_optional_idempotency(value)?.is_some() {
        return Err(Status::unimplemented(format!(
            "{rpc} idempotency_key replay is not wired yet"
        )));
    }
    Ok(())
}

fn encode_uuid_text(value: &str) -> Result<Vec<u8>, Status> {
    Uuid::parse_str(value)
        .map(|uuid| uuid.as_bytes().to_vec())
        .map_err(|err| Status::internal(format!("stored UUID text is not a UUID: {err}")))
}

fn decode_text_id(value: &[u8], field: &str) -> Result<String, Status> {
    String::from_utf8(value.to_vec())
        .map_err(|err| Status::invalid_argument(format!("{field} is not utf-8: {err}")))
}

pub(crate) fn timestamp_from_rfc3339(value: &str) -> Option<prost_types::Timestamp> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    Some(prost_types::Timestamp {
        seconds: parsed.unix_timestamp(),
        nanos: parsed.nanosecond() as i32,
    })
}

fn alarm_record_to_proto(record: remanence_state::AlarmRecord) -> pb::Alarm {
    pb::Alarm {
        alarm_id: u64::try_from(record.alarm_id).unwrap_or_default(),
        condition_key: record.condition_key,
        kind: record.kind,
        severity: record.severity,
        state: record.state,
        first_seen_utc: timestamp_from_rfc3339(record.first_seen_utc.as_str()),
        last_seen_utc: timestamp_from_rfc3339(record.last_seen_utc.as_str()),
        acked_by: record.acked_by.unwrap_or_default(),
        acked_at_utc: record
            .acked_at_utc
            .as_deref()
            .and_then(timestamp_from_rfc3339),
        detail: record.detail.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::fmt;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex};

    use ciborium::value::Value as CborValue;
    use remanence_aead::RootKey;
    #[cfg(feature = "foreign-bru")]
    use remanence_bru::{bru_checksum, BRU_BLOCK_SIZE};
    use remanence_format::{read_encrypted_rao_object, read_rem_tar_object};
    use remanence_library::scsi::{DeviceType, Inquiry};
    use remanence_library::{
        BlockSink, DiscoveryReport, DriveBay, ElementLayout, IdentitySource, IePort,
        InstalledDrive, Library, Slot, TapeIoError, TapePosition, VecBlockSink, VecBlockSource,
        WriteFilemarksOutcome, WriteOutcome,
    };
    use remanence_parity::bootstrap::{
        parse_bootstrap_block, write_bootstrap_block, BootstrapPayload,
    };
    use remanence_parity::{
        CommittedBundle, CommittedBundleKind, CommittedState, ParityConfig, ParityScheme, SchemeId,
        TapeFileEntry, TapeFileKind,
    };
    use remanence_state::{
        watermark_floor_bytes, AuditActor, AuditEvent, AuditRecord, AuditSubject,
        ForeignArchiveProjectionInput, NativeObjectCopyProjectionInput,
        NativeObjectFileProjectionInput, NativeObjectProjectionInput, PoolSelectionPolicyName,
        ProvisionTapeInput, SourceLayer, TapeJournalIndexInput, TapePoolProjectionInput,
        OBJECT_COPY_REPRESENTATION_ENCRYPTED, OBJECT_COPY_REPRESENTATION_PLAINTEXT,
    };
    use remanence_stream::{restore_object_to_directory, FilesystemRestoreOptions};
    use sha2::{Digest, Sha256};
    use tokio_stream::StreamExt;
    use tracing::dispatcher::Dispatch;
    use tracing::field::{Field, Visit};
    use tracing::metadata::LevelFilter;
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Subscriber};

    use super::*;

    const OBJECT_ID_TEXT: &str = "11111111-1111-1111-1111-111111111111";
    const OPERATION_ID_TEXT: &str = "22222222-2222-2222-2222-222222222222";
    const TAPE_UUID: [u8; 16] = [3u8; 16];
    const POOL_WRITE_TAPE_UUID: [u8; 16] = [4u8; 16];
    const SECOND_POOL_WRITE_TAPE_UUID: [u8; 16] = [5u8; 16];
    const API_SESSION_BLOCK_SIZE: u32 = 4096;
    #[cfg(feature = "foreign-bru")]
    const CHKSUM_OFFSET: usize = 0x080;
    #[cfg(feature = "foreign-bru")]
    const MAGIC_OFFSET: usize = 0x0B0;
    #[cfg(feature = "foreign-bru")]
    const MAGIC_SIZE: usize = 4;
    #[cfg(feature = "foreign-bru")]
    const MAGIC_ARCHIVE_HEADER: u64 = 0x1234;
    #[cfg(feature = "foreign-bru")]
    const MAGIC_FILE_HEADER: u64 = 0x2345;
    #[cfg(feature = "foreign-bru")]
    const ARTIME_OFFSET: usize = 0x098;
    #[cfg(feature = "foreign-bru")]
    const BUFSIZE_OFFSET: usize = 0x0A0;
    #[cfg(feature = "foreign-bru")]
    const RELEASE_MINOR_OFFSET: usize = 0x0B8;
    #[cfg(feature = "foreign-bru")]
    const RELEASE_MAJOR_OFFSET: usize = 0x0BC;
    #[cfg(feature = "foreign-bru")]
    const VARIANT_OFFSET: usize = 0x0C0;
    #[cfg(feature = "foreign-bru")]
    const ARCHIVE_ID_LOW_OFFSET: usize = 0x0D8;
    #[cfg(feature = "foreign-bru")]
    const LABEL_OFFSET: usize = 0x1D0;
    #[cfg(feature = "foreign-bru")]
    const FILE_PATH_OFFSET: usize = 0x000;
    #[cfg(feature = "foreign-bru")]
    const INLINE_DATA_LEN_OFFSET: usize = 0x0DC;
    #[cfg(feature = "foreign-bru")]
    const INLINE_DATA_OFFSET: usize = 0x400;
    #[cfg(feature = "foreign-bru")]
    const ST_MODE_OFFSET: usize = 0x180;
    #[cfg(feature = "foreign-bru")]
    const ST_SIZE_OFFSET: usize = 0x1B8;
    #[cfg(feature = "foreign-bru")]
    const S_IFREG: u64 = 0x8000;

    struct WarningCaptureSubscriber {
        messages: Arc<Mutex<Vec<String>>>,
        next_span_id: AtomicU64,
    }

    impl WarningCaptureSubscriber {
        fn new(messages: Arc<Mutex<Vec<String>>>) -> Self {
            Self {
                messages,
                next_span_id: AtomicU64::new(1),
            }
        }
    }

    struct WarningMessageVisitor {
        message: Option<String>,
    }

    impl WarningMessageVisitor {
        fn new() -> Self {
            Self { message: None }
        }
    }

    impl Visit for WarningMessageVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "message" {
                self.message = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            if field.name() == "message" {
                self.message = Some(format!("{value:?}"));
            }
        }
    }

    impl Subscriber for WarningCaptureSubscriber {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::WARN
        }

        fn new_span(&self, _attrs: &Attributes<'_>) -> Id {
            Id::from_u64(self.next_span_id.fetch_add(1, AtomicOrdering::Relaxed))
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            if *event.metadata().level() > tracing::Level::WARN {
                return;
            }
            let mut visitor = WarningMessageVisitor::new();
            event.record(&mut visitor);
            if let Some(message) = visitor.message {
                self.messages
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .push(message);
            }
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}

        fn register_callsite(
            &self,
            metadata: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            if *metadata.level() <= tracing::Level::WARN {
                tracing::subscriber::Interest::always()
            } else {
                tracing::subscriber::Interest::never()
            }
        }

        fn max_level_hint(&self) -> Option<LevelFilter> {
            Some(LevelFilter::WARN)
        }
    }

    fn capture_warnings<F>(f: F) -> Vec<String>
    where
        F: FnOnce(),
    {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let subscriber = WarningCaptureSubscriber::new(Arc::clone(&messages));
        tracing::dispatcher::with_default(&Dispatch::new(subscriber), f);
        Arc::try_unwrap(messages)
            .expect("warning capture has one owner")
            .into_inner()
            .expect("warning capture mutex not poisoned")
    }

    fn test_index() -> CatalogIndex {
        let dir = std::env::temp_dir().join(format!("remanence-api-{}", Uuid::new_v4()));
        CatalogIndex::open(dir.join("state.sqlite")).expect("open test index")
    }

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[tokio::test]
    async fn connect_unix_to_missing_socket_fails() {
        let missing = temp_dir("remanence-api-missing-socket").join("nope.sock");
        let result = crate::connect_unix(missing).await;
        assert!(result.is_err(), "connecting to a missing socket must error");
    }

    #[test]
    fn append_spool_cap_clamps_client_declared_size() {
        assert_eq!(append_spool_cap(0), crate::write_owner::SPOOL_MAX_BYTES);
        assert_eq!(append_spool_cap(1024), 1024);
        assert_eq!(
            append_spool_cap(u64::MAX),
            crate::write_owner::SPOOL_MAX_BYTES
        );
    }

    #[test]
    fn spool_budget_permits_rounds_up_to_mib_units() {
        assert_eq!(spool_budget_permits(0), 1);
        assert_eq!(spool_budget_permits(1), 1);
        assert_eq!(spool_budget_permits(1024 * 1024), 1);
        assert_eq!(spool_budget_permits(1024 * 1024 + 1), 2);
        assert_eq!(
            spool_budget_permits(crate::write_owner::SPOOL_MAX_BYTES),
            64 * 1024
        );
    }

    #[test]
    fn append_spool_write_error_mapping_keeps_io_errors_distinct() {
        let cap = status_from_append_spool_write_error(io::Error::new(
            io::ErrorKind::InvalidInput,
            "spool size cap exceeded",
        ));
        assert_eq!(cap.code(), tonic::Code::ResourceExhausted);
        assert!(cap.message().contains("cap"));

        let io_error = status_from_append_spool_write_error(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "permission denied",
        ));
        assert_eq!(io_error.code(), tonic::Code::Internal);
        assert!(io_error.message().contains("write append spool"));
    }

    #[test]
    fn actor_from_request_uses_metadata_fallback_when_no_peer_cert() {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("x-remanence-actor", "operator-a".parse().unwrap());

        assert_eq!(
            actor_from_request(&request),
            AuditActor::Service("operator-a".to_string())
        );
    }

    #[test]
    fn actor_from_request_defaults_to_system_without_identity() {
        assert_eq!(actor_from_request(&Request::new(())), AuditActor::System);
    }

    #[test]
    fn auth_role_parser_accepts_spec_roles_and_subject_prefixes() {
        assert_eq!(parse_client_role("readonly"), Some(ClientRole::Readonly));
        assert_eq!(
            parse_client_role("role:orchestrator"),
            Some(ClientRole::Orchestrator)
        );
        assert_eq!(
            parse_client_role("remanence-role=operator"),
            Some(ClientRole::Operator)
        );
        assert_eq!(
            parse_client_role("Role:Operator"),
            Some(ClientRole::Operator)
        );
        assert_eq!(parse_client_role("admin"), Some(ClientRole::Admin));
        assert_eq!(parse_client_role("system"), Some(ClientRole::System));
        assert_eq!(parse_client_role("writer"), None);
    }

    #[test]
    fn certificate_role_requires_remanence_prefix() {
        assert_eq!(
            parse_certificate_role_attribute("remanence-role=operator"),
            Some(ClientRole::Operator)
        );
        assert_eq!(
            parse_certificate_role_attribute("Remanence-Role:Admin"),
            Some(ClientRole::Admin)
        );
        // A human-chosen subject value must never grant a role from a
        // certificate: bare words and generic prefixes are rejected.
        assert_eq!(parse_certificate_role_attribute("operator"), None);
        assert_eq!(parse_certificate_role_attribute("admin"), None);
        assert_eq!(parse_certificate_role_attribute("role=admin"), None);
        assert_eq!(parse_certificate_role_attribute("role:operator"), None);
    }

    #[test]
    fn auth_role_parser_reads_mtls_certificate_subject() {
        // CN = "remanence-role=orchestrator" — the only certificate
        // form that grants a role.
        const CERT: &[u8] = b"-----BEGIN CERTIFICATE-----
MIICKDCCAZGgAwIBAgIUSC6Pz9m7L+r7OACC/z3EyzxjlukwDQYJKoZIhvcNAQEL
BQAwJjEkMCIGA1UEAwwbcmVtYW5lbmNlLXJvbGU9b3JjaGVzdHJhdG9yMB4XDTI2
MDYxMDA5MzYxM1oXDTI3MDYxMDA5MzYxM1owJjEkMCIGA1UEAwwbcmVtYW5lbmNl
LXJvbGU9b3JjaGVzdHJhdG9yMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDT
Oj3oJ5Mj+bwA9KUTNWM6Sn7085JZJFyWXFYnnTCXGeQKcFB/4hWtNT4RzNOPOuHE
yenUAdnjERB0Q88+ZGiCFW0a7mqVgGvIQ0ALe5hUtDbr1C/L5PVnTPdJL6qx05tW
AFKiFiSgTZCf5jXmUL8ijJk6PwaWsziX78aowc8ahQIDAQABo1MwUTAdBgNVHQ4E
FgQUC6w9intd3BWy5ndUax7FvPuFys0wHwYDVR0jBBgwFoAUC6w9intd3BWy5ndU
ax7FvPuFys0wDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOBgQBHhl8C
ut8itrK85Q5dfBXf9PF+VO2mBDwygxFHq2zGc7h+adH22nDP5O9ruYp0f6CO/YE+
UCR1Of7847/e0wZzH2MZWiSxwbcPPO9IbLLfJcL9+WOZDuLlbJOlSW3fsQjuCK/3
0BJvX603jdLLX35ExjbI9rZf+ljSS7BGLFDHBA==
-----END CERTIFICATE-----
";
        let (_, pem) = x509_parser::pem::parse_x509_pem(CERT).expect("parse pem");

        assert_eq!(
            role_from_certificate_subject(&pem.contents),
            Some(ClientRole::Orchestrator)
        );
    }

    #[test]
    fn auth_role_certificate_ignores_unprefixed_subject_values() {
        // CN = "role:orchestrator" — the generic prefix (and any bare
        // role word) must NOT grant a role from a certificate subject;
        // such a client falls back to the Readonly default.
        const CERT: &[u8] = b"-----BEGIN CERTIFICATE-----
MIICFDCCAX2gAwIBAgIUWUo200SX/lizn4w3+toMUqWGebAwDQYJKoZIhvcNAQEL
BQAwHDEaMBgGA1UEAwwRcm9sZTpvcmNoZXN0cmF0b3IwHhcNMjYwNjEwMDYwOTQ1
WhcNMjYwNjExMDYwOTQ1WjAcMRowGAYDVQQDDBFyb2xlOm9yY2hlc3RyYXRvcjCB
nzANBgkqhkiG9w0BAQEFAAOBjQAwgYkCgYEAvlexVTSFywY/KmuOrb/JcWHZRe+k
+4xTSSoli2GPVtLLbtG20P8M2f3ztgmspofWYEHizDTAazEDUVpuNVMArHxtCYkl
F870VaNGqNLQbuO7RTuxZBdBsPx53r4r9+y98EoMXaIDY9fr+KLHCVbRM95fdVoE
SbZhirGgZDzedZUCAwEAAaNTMFEwHQYDVR0OBBYEFErl+mpvQQw8/j/Wtwleg0Hj
SuTbMB8GA1UdIwQYMBaAFErl+mpvQQw8/j/Wtwleg0HjSuTbMA8GA1UdEwEB/wQF
MAMBAf8wDQYJKoZIhvcNAQELBQADgYEAp1gfrStgB/mqWv9CEp5RN4zzHRK4M52m
Hr4Eecw8Zz+C5rD4eTUvlTEVUuOHsHkXm3/KYkp5Emw3ncNvtjnrc5eKRalaj59Z
hZqLlGKuZLlibfY5VIYyxzQ1tuZlG7PFCKFjOmT8xoY7/nysfaITmwD7JazQPELZ
BCw3Wyv2UWY=
-----END CERTIFICATE-----
";
        let (_, pem) = x509_parser::pem::parse_x509_pem(CERT).expect("parse pem");

        assert_eq!(role_from_certificate_subject(&pem.contents), None);
    }

    #[test]
    fn authorization_allows_readonly_reads_but_denies_writes() {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("x-remanence-role", "readonly".parse().unwrap());

        assert!(authorize_request(&request, AuthPermission::Read).is_ok());
        assert!(authorize_request(&request, AuthPermission::ReadTape).is_ok());
        assert_eq!(
            authorize_request(&request, AuthPermission::Write)
                .expect_err("readonly must not write")
                .code(),
            tonic::Code::PermissionDenied
        );
    }

    #[test]
    fn authorization_denies_operator_write_but_allows_robotics() {
        let mut request = Request::new(());
        request
            .metadata_mut()
            .insert("x-remanence-role", "operator".parse().unwrap());

        assert!(authorize_request(&request, AuthPermission::Robotics).is_ok());
        assert_eq!(
            authorize_request(&request, AuthPermission::Write)
                .expect_err("operator must not write")
                .code(),
            tonic::Code::PermissionDenied
        );
    }

    #[test]
    fn authorization_matrix_covers_drive_stewardship_mutations() {
        let cases = [
            (ClientRole::System, true, true, true),
            (ClientRole::Admin, true, true, false),
            (ClientRole::Orchestrator, true, true, false),
            (ClientRole::Operator, false, true, false),
            (ClientRole::Readonly, false, false, false),
        ];
        for (role, annotate, robotics, lifecycle) in cases {
            assert_eq!(
                role.allows(AuthPermission::Write),
                annotate,
                "{role:?} AnnotateDrive/Write"
            );
            assert_eq!(
                role.allows(AuthPermission::Robotics),
                robotics,
                "{role:?} PollDrive/CleanDrive/AckAlarm/Robotics"
            );
            assert_eq!(
                role.allows(AuthPermission::Lifecycle),
                lifecycle,
                "{role:?} RetireDrive/Lifecycle"
            );
        }
    }

    #[tokio::test]
    async fn write_session_rejects_readonly_role_before_validation() {
        let service = ApiState::new(test_index()).write_session_service();
        let mut request = Request::new(pb::OpenWriteSessionRequest {
            target: None,
            body_format: String::new(),
            idempotency_key: None,
            recover_session_id: Vec::new(),
        });
        request
            .metadata_mut()
            .insert("x-remanence-role", "readonly".parse().unwrap());

        let err = pb::write_session_service_server::WriteSessionService::open_write_session(
            &service, request,
        )
        .await
        .expect_err("readonly write must be rejected before request validation");
        assert_eq!(err.code(), tonic::Code::PermissionDenied);
    }

    #[test]
    fn unimplemented_idempotency_rejects_only_non_empty_keys() {
        reject_unimplemented_idempotency(None, "TestRpc").expect("absent key");
        reject_unimplemented_idempotency(
            Some(&pb::IdempotencyKey { value: Vec::new() }),
            "TestRpc",
        )
        .expect("empty key");

        let err = reject_unimplemented_idempotency(
            Some(&pb::IdempotencyKey { value: vec![1] }),
            "TestRpc",
        )
        .expect_err("malformed key rejected before feature gate");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        let err = reject_unimplemented_idempotency(
            Some(&pb::IdempotencyKey {
                value: Uuid::new_v4().as_bytes().to_vec(),
            }),
            "TestRpc",
        )
        .expect_err("non-empty valid key is not silently accepted");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    #[tokio::test]
    async fn write_session_rejects_idempotency_key_before_validation() {
        let service = ApiState::new(test_index()).write_session_service();
        let err = pb::write_session_service_server::WriteSessionService::open_write_session(
            &service,
            Request::new(pb::OpenWriteSessionRequest {
                target: None,
                body_format: String::new(),
                idempotency_key: Some(pb::IdempotencyKey {
                    value: Uuid::new_v4().as_bytes().to_vec(),
                }),
                recover_session_id: Vec::new(),
            }),
        )
        .await
        .expect_err("non-enforced idempotency key must fail before dispatch");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    #[test]
    fn cancel_audit_records_request_actor() {
        let state = ApiState::new(test_index());
        let operation_id = Uuid::new_v4();
        let actor = AuditActor::Service("operator-a".to_string());

        state
            .record_cancel_requested(actor.clone(), operation_id, None, false)
            .expect("record cancel");

        let records = FileAuditLog::replay(state.audit_dir.as_ref()).expect("replay audit");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].actor, actor);
        assert_eq!(records[0].event, AuditEvent::CancelRequested);
    }

    #[test]
    fn library_request_audit_records_request_actor() {
        let state = ApiState::new(test_index());
        let operation_id = Uuid::new_v4();
        let actor = AuditActor::Service("operator-a".to_string());

        state
            .record_library_request_received(
                actor.clone(),
                operation_id,
                "refresh_inventory",
                "LIB001",
                BTreeMap::new(),
            )
            .expect("record library request");

        let records = FileAuditLog::replay(state.audit_dir.as_ref()).expect("replay audit");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].actor, actor);
        assert_eq!(records[0].event, AuditEvent::RequestReceived);
        assert_eq!(records[0].subject.kind, "library");
        assert_eq!(records[0].subject.id.as_deref(), Some("LIB001"));
    }

    fn test_scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("rs-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 3,
        }
    }

    fn project_pool(index: &mut CatalogIndex, pool_id: &str) {
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: pool_id.to_string(),
                display_name: Some(pool_id.to_string()),
                copy_class: None,
                content_class: None,
                created_at_utc: Some("2026-05-28T09:00:00Z".to_string()),
            })
            .expect("project pool");
    }

    fn pool_config(pool_id: &str) -> TapePoolConfig {
        TapePoolConfig {
            id: pool_id.trim().to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: PoolSelectionPolicyName::CompleteOrFill,
            watermark_low: 0.92,
            watermark_high: 0.97,
            block_size_bytes: u64::from(API_SESSION_BLOCK_SIZE),
            min_object_size_bytes: 0,
        }
    }

    fn pool_config_with_block_size(pool_id: &str, block_size: u32) -> TapePoolConfig {
        let mut cfg = pool_config(pool_id);
        cfg.block_size_bytes = u64::from(block_size);
        cfg
    }

    fn pool_config_with_watermarks(
        pool_id: &str,
        watermark_low: f64,
        watermark_high: f64,
        min_object_size_bytes: u64,
    ) -> TapePoolConfig {
        TapePoolConfig {
            id: pool_id.to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            selection_policy: PoolSelectionPolicyName::CompleteOrFill,
            watermark_low,
            watermark_high,
            block_size_bytes: u64::from(API_SESSION_BLOCK_SIZE),
            min_object_size_bytes,
        }
    }

    fn project_eligible_tape(index: &mut CatalogIndex, pool_id: &str, tape_uuid: [u8; 16]) {
        project_eligible_tape_with_voltag(
            index,
            pool_id,
            tape_uuid,
            format!("RMN{:03}L9", tape_uuid[0]).as_str(),
        );
    }

    fn project_eligible_tape_with_voltag(
        index: &mut CatalogIndex,
        pool_id: &str,
        tape_uuid: [u8; 16],
        voltag: &str,
    ) {
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: voltag.to_string(),
                block_size: API_SESSION_BLOCK_SIZE,
                parity: ParityConfig::Scheme(test_scheme()),
                force: false,
            })
            .expect("provision parity tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign tape to pool");
    }

    fn project_ready_tape_usage(
        index: &mut CatalogIndex,
        tape_uuid: [u8; 16],
        total_committed_ordinals: u64,
    ) {
        index
            .project_committed_tape_file_bundle(
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: API_SESSION_BLOCK_SIZE,
                    scheme: Some(test_scheme()),
                    journal_offset_bytes: 0,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 1,
                        kind: TapeFileKind::Object,
                        block_count: total_committed_ordinals,
                        physical_start_hint: Some(0),
                        object_id: None,
                        first_parity_data_ordinal: Some(0),
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: None,
                    }],
                    highest_protected_ordinal: 0,
                    total_committed_ordinals,
                },
            )
            .expect("project ready tape usage");
    }

    fn project_no_parity_tape_usage(
        index: &mut CatalogIndex,
        tape_uuid: [u8; 16],
        total_committed_ordinals: u64,
    ) {
        index
            .project_committed_tape_file_bundle(
                TapeJournalIndexInput {
                    tape_uuid,
                    block_size: API_SESSION_BLOCK_SIZE,
                    scheme: None,
                    journal_offset_bytes: 0,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 1,
                        kind: TapeFileKind::Object,
                        block_count: total_committed_ordinals,
                        physical_start_hint: Some(0),
                        object_id: None,
                        first_parity_data_ordinal: None,
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: None,
                    }],
                    highest_protected_ordinal: 0,
                    total_committed_ordinals,
                },
            )
            .expect("project no-parity tape usage");
    }

    fn project_no_parity_tape(index: &mut CatalogIndex, pool_id: &str, tape_uuid: [u8; 16]) {
        project_no_parity_tape_with_block_size(index, pool_id, tape_uuid, API_SESSION_BLOCK_SIZE);
    }

    fn project_no_parity_tape_with_block_size(
        index: &mut CatalogIndex,
        pool_id: &str,
        tape_uuid: [u8; 16],
        block_size: u32,
    ) {
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: format!("RMN{:03}L9", tape_uuid[0]),
                block_size,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision no-parity tape");
        index
            .project_tape_pool_membership(tape_uuid, pool_id)
            .expect("assign no-parity tape to pool");
    }

    fn no_parity_bootstrap_block(tape_uuid: [u8; 16]) -> Vec<u8> {
        let payload = BootstrapPayload {
            scheme: None,
            no_parity_flag: true,
            filemark_map_digest: None,
            tape_uuid,
            written_by_version: "test".to_string(),
            written_at: "2026-05-29T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: API_SESSION_BLOCK_SIZE,
            drive_compression: false,
            sidecar_epoch_directory: None,
            parity_map_reference: None,
            object_rows: Vec::new(),
        };
        let mut block = vec![0u8; API_SESSION_BLOCK_SIZE as usize];
        write_bootstrap_block(&payload, &mut block).expect("write no-parity bootstrap");
        block
    }

    fn writable_tape_record() -> TapeRecord {
        TapeRecord {
            tape_uuid: POOL_WRITE_TAPE_UUID.to_vec(),
            voltag: Some("RMN001L9".to_string()),
            kind: "data".to_string(),
            pool_id: Some("camera.copy-a".to_string()),
            body_format: None,
            block_size: Some(API_SESSION_BLOCK_SIZE as u64),
            scheme_id: None,
            data_blocks_per_stripe: None,
            parity_blocks_per_stripe: None,
            stripes_per_neighborhood: None,
            last_committed_tape_file: None,
            total_committed_ordinals: 0,
            state: "ready".to_string(),
            updated_at_utc: "2026-05-29T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn bootstrap_build_write_parse_round_trips_no_parity_and_parity() {
        let no_parity = build_tape_bootstrap(
            POOL_WRITE_TAPE_UUID,
            API_SESSION_BLOCK_SIZE,
            ParityConfig::None,
            "2026-05-29T00:00:00Z",
            "test-version",
        );
        let mut no_parity_sink = VecBlockSink::new();
        write_tape_bootstrap(&mut no_parity_sink, &no_parity).expect("write no-parity bootstrap");
        assert_eq!(no_parity_sink.filemarks, vec![1]);
        let parsed =
            parse_bootstrap_block(&no_parity_sink.blocks[0]).expect("parse no-parity bootstrap");
        assert_eq!(parsed.tape_uuid, POOL_WRITE_TAPE_UUID);
        assert_eq!(parsed.block_size_bytes, API_SESSION_BLOCK_SIZE);
        assert!(parsed.no_parity_flag);
        assert!(parsed.scheme.is_none());
        assert!(parsed.filemark_map_digest.is_none());

        let parity = build_tape_bootstrap(
            SECOND_POOL_WRITE_TAPE_UUID,
            API_SESSION_BLOCK_SIZE,
            ParityConfig::Scheme(test_scheme()),
            "2026-05-29T00:00:00Z",
            "test-version",
        );
        let mut parity_sink = VecBlockSink::new();
        write_tape_bootstrap(&mut parity_sink, &parity).expect("write parity bootstrap");
        assert_eq!(parity_sink.filemarks, vec![1]);
        let parsed = parse_bootstrap_block(&parity_sink.blocks[0]).expect("parse parity bootstrap");
        assert_eq!(parsed.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
        assert_eq!(parsed.block_size_bytes, API_SESSION_BLOCK_SIZE);
        assert!(!parsed.no_parity_flag);
        assert!(parsed.scheme.is_some());
        assert!(parsed.filemark_map_digest.is_some());
    }

    #[test]
    fn lto_capacity_table_parses_suffixes_and_reports_raw_capacity() {
        assert_eq!(lto_generation_from_voltag("RMN001L9"), Some(LtoGen::Lto9));
        assert_eq!(lto_generation_from_voltag("rmn002l8"), Some(LtoGen::Lto8));
        assert_eq!(lto_generation_from_voltag("RMN003L7"), Some(LtoGen::Lto7));
        assert_eq!(lto_generation_from_voltag("RMN004"), None);
        assert_eq!(raw_capacity_bytes(LtoGen::Lto7), 6_000_000_000_000);
        assert_eq!(raw_capacity_bytes(LtoGen::Lto8), 12_000_000_000_000);
        assert_eq!(raw_capacity_bytes(LtoGen::Lto9), 18_000_000_000_000);
    }

    #[test]
    fn writability_preconditions_accept_ready_tape_and_report_each_reject() {
        let tape = writable_tape_record();
        check_writability_preconditions(&tape, 1024).expect("ready tape is writable");

        let mut not_ready = tape.clone();
        not_ready.state = "ingested".to_string();
        let err = check_writability_preconditions(&not_ready, 1024).expect_err("not ready rejects");
        assert!(
            matches!(err, WritabilityError::NotReady { ref state } if state == "ingested"),
            "{err}"
        );

        let mut missing_geometry = tape.clone();
        missing_geometry.scheme_id = Some("rs-test".to_string());
        let err = check_writability_preconditions(&missing_geometry, 1024)
            .expect_err("partial parity geometry rejects");
        assert!(
            matches!(err, WritabilityError::MissingGeometry { .. }),
            "{err}"
        );

        let mut exhausted = tape;
        exhausted.voltag = Some("RMN001L1".to_string());
        exhausted.block_size = Some(100);
        let scheme = test_scheme();
        exhausted.scheme_id = Some(scheme.id.as_str().to_string());
        exhausted.data_blocks_per_stripe = Some(u32::from(scheme.data_blocks_per_stripe));
        exhausted.parity_blocks_per_stripe = Some(u32::from(scheme.parity_blocks_per_stripe));
        exhausted.stripes_per_neighborhood = Some(scheme.stripes_per_neighborhood);
        let err = check_writability_preconditions(
            &exhausted,
            raw_capacity_bytes(LtoGen::Lto1).saturating_add(1),
        )
        .expect_err("capacity rejects");
        assert!(
            matches!(err, WritabilityError::InsufficientCapacity { .. }),
            "{err}"
        );

        let mut written_no_parity = writable_tape_record();
        written_no_parity.total_committed_ordinals = 7;
        let err = check_writability_preconditions(&written_no_parity, 1)
            .expect_err("written no-parity tape must not be reused");
        assert!(
            matches!(
                err,
                WritabilityError::NoParityAppendUnsupported {
                    total_committed_ordinals: 7
                }
            ),
            "{err}"
        );

        let mut written_parity = writable_tape_record();
        let scheme = test_scheme();
        written_parity.scheme_id = Some(scheme.id.as_str().to_string());
        written_parity.data_blocks_per_stripe = Some(u32::from(scheme.data_blocks_per_stripe));
        written_parity.parity_blocks_per_stripe = Some(u32::from(scheme.parity_blocks_per_stripe));
        written_parity.stripes_per_neighborhood = Some(scheme.stripes_per_neighborhood);
        written_parity.total_committed_ordinals = 11;
        let err = check_writability_preconditions(&written_parity, 1)
            .expect_err("written parity tape must not be reopened at BOT");
        assert!(
            matches!(
                err,
                WritabilityError::ParityAppendUnsupported {
                    total_committed_ordinals: 11
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn retired_tape_is_rejected_as_not_ready_for_writes() {
        let mut retired = writable_tape_record();
        retired.state = "retired".to_string();

        let err = check_writability_preconditions(&retired, 1024)
            .expect_err("retired tape must reject writes");

        assert!(
            matches!(err, WritabilityError::NotReady { ref state } if state == "retired"),
            "{err}"
        );
    }

    fn object_uuid() -> Uuid {
        Uuid::parse_str(OBJECT_ID_TEXT).expect("valid object uuid")
    }

    fn operation_uuid() -> Uuid {
        Uuid::parse_str(OPERATION_ID_TEXT).expect("valid operation uuid")
    }

    fn populated_state() -> ApiState {
        let mut index = test_index();
        let scheme = ParityScheme {
            id: SchemeId::new_static("rs-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 3,
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
                tape_uuid: TAPE_UUID,
                voltag: "ACM003L9".to_string(),
                block_size: 4096,
                parity: ParityConfig::Scheme(scheme.clone()),
                force: false,
            })
            .expect("provision tape before assigning pool");
        index
            .project_tape_pool_membership(TAPE_UUID, "camera.copy-a")
            .expect("assign tape to pool");
        index
            .index_committed_tape_journal(
                TapeJournalIndexInput {
                    tape_uuid: TAPE_UUID,
                    block_size: 4096,
                    scheme: Some(scheme),
                    journal_offset_bytes: 99,
                },
                &CommittedState {
                    entries: vec![
                        TapeFileEntry {
                            tape_file_number: 4,
                            kind: TapeFileKind::Object,
                            block_count: 5,
                            physical_start_hint: Some(0),
                            object_id: Some(OBJECT_ID_TEXT.to_string()),
                            first_parity_data_ordinal: Some(0),
                            epoch_id: None,
                            protected_ordinal_start: None,
                            protected_ordinal_end_exclusive: None,
                            canonical_metadata_hash: None,
                            bootstrap_object_row: None,
                        },
                        TapeFileEntry {
                            tape_file_number: 5,
                            kind: TapeFileKind::ParitySidecar,
                            block_count: 2,
                            physical_start_hint: Some(5),
                            object_id: None,
                            first_parity_data_ordinal: None,
                            epoch_id: Some(0),
                            protected_ordinal_start: Some(0),
                            protected_ordinal_end_exclusive: Some(5),
                            canonical_metadata_hash: Some([9u8; 32]),
                            bootstrap_object_row: None,
                        },
                        TapeFileEntry {
                            tape_file_number: 6,
                            kind: TapeFileKind::ParityMap,
                            block_count: 1,
                            physical_start_hint: Some(7),
                            object_id: None,
                            first_parity_data_ordinal: None,
                            epoch_id: Some(0),
                            protected_ordinal_start: Some(0),
                            protected_ordinal_end_exclusive: Some(5),
                            canonical_metadata_hash: Some([8u8; 32]),
                            bootstrap_object_row: None,
                        },
                        TapeFileEntry {
                            tape_file_number: 7,
                            kind: TapeFileKind::Bootstrap,
                            block_count: 1,
                            physical_start_hint: Some(8),
                            object_id: None,
                            first_parity_data_ordinal: None,
                            epoch_id: None,
                            protected_ordinal_start: None,
                            protected_ordinal_end_exclusive: None,
                            canonical_metadata_hash: Some([7u8; 32]),
                            bootstrap_object_row: None,
                        },
                    ],
                    highest_protected_ordinal: 5,
                    total_committed_ordinals: 5,
                },
            )
            .expect("index tape journal");
        index
            .upsert_native_object_projection(
                NativeObjectProjectionInput {
                    object_id: OBJECT_ID_TEXT.to_string(),
                    caller_object_id: Some("caller-1".to_string()),
                    body_format: "rao-v1".to_string(),
                    logical_size_bytes: Some(17),
                    content_hash: Some(vec![7u8; 32]),
                    metadata_hash: None,
                    created_at_utc: Some("2026-05-28T12:00:00Z".to_string()),
                },
                &[NativeObjectCopyProjectionInput {
                    object_id: OBJECT_ID_TEXT.to_string(),
                    tape_uuid: TAPE_UUID,
                    tape_file_number: 4,
                    first_body_lba: 0,
                    first_parity_data_ordinal: Some(0),
                    protected_until_ordinal: Some(8),
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                    key_id: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x51; 32]),
                    stored_digest: Some(vec![0x51; 32]),
                }],
            )
            .expect("populate object");
        ApiState::new(index)
    }

    fn populated_state_with_file_catalog() -> ApiState {
        let state = populated_state();
        let scheme = ParityScheme {
            id: SchemeId::new_static("rs-test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 3,
        };
        let mut index = CatalogIndex::open(state.index_path.as_ref()).expect("open test index");
        index
            .project_native_object_and_committed_tape_file_bundle(
                NativeObjectProjectionInput {
                    object_id: OBJECT_ID_TEXT.to_string(),
                    caller_object_id: Some("caller-1".to_string()),
                    body_format: "rao-v1".to_string(),
                    logical_size_bytes: Some(17),
                    content_hash: Some(vec![7u8; 32]),
                    metadata_hash: None,
                    created_at_utc: Some("2026-05-28T12:00:00Z".to_string()),
                },
                &[NativeObjectFileProjectionInput {
                    object_id: OBJECT_ID_TEXT.to_string(),
                    file_id: "file-camera".to_string(),
                    path: "payload.bin".to_string(),
                    size_bytes: 17,
                    file_sha256: vec![7u8; 32],
                    first_chunk_lba: Some(2),
                    chunk_count: 1,
                    mtime: Some("0".to_string()),
                    executable: Some(false),
                }],
                &[NativeObjectCopyProjectionInput {
                    object_id: OBJECT_ID_TEXT.to_string(),
                    tape_uuid: TAPE_UUID,
                    tape_file_number: 4,
                    first_body_lba: 0,
                    first_parity_data_ordinal: Some(0),
                    protected_until_ordinal: Some(8),
                    status: "committed".to_string(),
                    representation: OBJECT_COPY_REPRESENTATION_PLAINTEXT.to_string(),
                    key_id: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x51; 32]),
                    stored_digest: Some(vec![0x51; 32]),
                }],
                TapeJournalIndexInput {
                    tape_uuid: TAPE_UUID,
                    block_size: 4096,
                    scheme: Some(scheme),
                    journal_offset_bytes: 99,
                },
                &CommittedBundle {
                    kind: CommittedBundleKind::Object,
                    entries: vec![TapeFileEntry {
                        tape_file_number: 4,
                        kind: TapeFileKind::Object,
                        block_count: 5,
                        physical_start_hint: Some(0),
                        object_id: Some(OBJECT_ID_TEXT.to_string()),
                        first_parity_data_ordinal: Some(0),
                        epoch_id: None,
                        protected_ordinal_start: None,
                        protected_ordinal_end_exclusive: None,
                        canonical_metadata_hash: None,
                        bootstrap_object_row: None,
                    }],
                    highest_protected_ordinal: 5,
                    total_committed_ordinals: 5,
                },
            )
            .expect("populate object file rows");
        state
    }

    fn empty_pool_state() -> ApiState {
        pool_state_with_tapes(&[TAPE_UUID])
    }

    fn state_with_library_snapshot(serial: &str) -> ApiState {
        let mut state = empty_pool_state();
        state.library_snapshot = Some(Arc::new(RwLock::new(Arc::new(LibrarySnapshot {
            report: DiscoveryReport {
                libraries: vec![test_library(serial)],
                warnings: Vec::new(),
            },
            captured_at: OffsetDateTime::UNIX_EPOCH,
        }))));
        state
    }

    fn test_library(serial: &str) -> Library {
        Library {
            serial: serial.to_string(),
            changer_sg: PathBuf::from("/dev/sg7"),
            changer_sysfs: PathBuf::from("/sys/test"),
            changer_inquiry: Inquiry {
                device_type: DeviceType::MediumChanger,
                peripheral_qualifier: 0,
                removable: true,
                version: 7,
                response_data_format: 2,
                additional_length: 31,
                vendor: *b"HPE     ",
                product: *b"MSL3040         ",
                revision: *b"6.40",
            },
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 1,
                drive_count: 1,
                slot_start: 0x03e8,
                slot_count: 1,
                ie_start: 0x10,
                ie_count: 1,
            },
            drive_bays: vec![DriveBay {
                element_address: 1,
                accessible: true,
                installed: Some(InstalledDrive {
                    serial: "8031BDC7D1".to_string(),
                    identity_source: IdentitySource::DvcidInline,
                    vendor: Some("HPE".to_string()),
                    product: Some("Ultrium 9-SCSI".to_string()),
                    revision: Some("R1.0".to_string()),
                    sg_path: Some(PathBuf::from("/dev/sg8")),
                    sysfs_path: None,
                }),
                loaded: false,
                loaded_tape: None,
                source_slot: None,
            }],
            slots: vec![Slot {
                element_address: 0x03e9,
                accessible: true,
                full: true,
                cartridge: Some("S30002L9".to_string()),
            }],
            ie_ports: vec![IePort {
                element_address: 0x10,
                accessible: true,
                full: false,
                cartridge: None,
                import_enabled: true,
                export_enabled: true,
            }],
        }
    }

    fn foreign_drive_library(serial: &str, bays: &[(u16, &str, Option<&str>)]) -> Library {
        let mut library = test_library(serial);
        library.layout.drive_start = bays
            .iter()
            .map(|(element_address, _, _)| *element_address)
            .min()
            .unwrap_or(0);
        library.layout.drive_count = u16::try_from(bays.len()).expect("test bay count fits u16");
        library.drive_bays = bays
            .iter()
            .map(|(element_address, drive_serial, sg_path)| DriveBay {
                element_address: *element_address,
                accessible: true,
                installed: Some(InstalledDrive {
                    serial: (*drive_serial).to_string(),
                    identity_source: IdentitySource::DvcidAndInquiry,
                    vendor: Some("IBM".to_string()),
                    product: Some("ULT3580".to_string()),
                    revision: Some("A1".to_string()),
                    sg_path: sg_path.map(PathBuf::from),
                    sysfs_path: None,
                }),
                loaded: false,
                loaded_tape: None,
                source_slot: None,
            })
            .collect();
        library
    }

    fn foreign_counter_snapshot(tape_alert_flags: Option<&str>) -> ForeignDriveSnapshot {
        ForeignDriveSnapshot {
            tape_alert_flags: tape_alert_flags.map(str::to_string),
            write_errors_corrected: Some(11),
            write_errors_uncorrected: Some(1),
            read_errors_corrected: Some(7),
            read_errors_uncorrected: Some(0),
        }
    }

    #[test]
    fn foreign_poll_skips_same_serial_collision_rows_without_attribution() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-foreign-collision")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let report = DiscoveryReport {
            libraries: vec![foreign_drive_library(
                "d2lib",
                &[
                    (0x0100, "DUPSER", Some("/dev/sg10")),
                    (0x0101, "DUPSER", Some("/dev/sg11")),
                ],
            )],
            warnings: Vec::new(),
        };
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        observe_drive_catalog_from_libraries(&mut index, report.libraries.iter(), &HashSet::new())
            .expect("observe duplicate serial foreign rows");
        let collision_rows = index.list_drives(true, false).expect("list drives");
        assert_eq!(collision_rows.len(), 2);
        assert!(
            collision_rows.iter().all(|drive| !drive.actionable),
            "duplicate serial rows must be non-actionable: {collision_rows:?}"
        );
        drop(index);

        let drives_cfg = remanence_state::DrivesConfig {
            foreign_tapealert: true,
            ..remanence_state::DrivesConfig::default()
        };
        let mut reads = Vec::new();
        poll_foreign_drive_counters_once_with_reader(
            &index_path,
            &report,
            &drives_cfg,
            &HashSet::new(),
            |path, _foreign_tapealert| {
                reads.push(path.to_path_buf());
                Ok(foreign_counter_snapshot(Some("[20]")))
            },
        )
        .expect("poll foreign counters");

        assert!(
            reads.is_empty(),
            "ambiguous duplicate serial bays must not be polled or attributed: {reads:?}"
        );
        let index = CatalogIndex::open(&index_path).expect("reopen catalog");
        for drive in collision_rows {
            assert!(
                index
                    .list_drive_health_snapshots(&drive.drive_uuid)
                    .expect("list snapshots")
                    .is_empty(),
                "collision row received a snapshot: {drive:?}"
            );
        }
        let active_alarms = index.list_alarms(false).expect("list active alarms");
        assert!(
            active_alarms
                .iter()
                .all(|alarm| alarm.kind != "foreign-drive-wants-cleaning"),
            "collision rows must not receive foreign cleaning advisories: {active_alarms:?}"
        );
    }

    #[test]
    fn foreign_poll_attributes_unambiguous_row_by_library_and_element_address() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-api-foreign-unambiguous")
            .tempdir()
            .expect("tempdir");
        let index_path = temp.path().join("rem-state.sqlite");
        let report = DiscoveryReport {
            libraries: vec![foreign_drive_library(
                "d2lib",
                &[
                    (0x0100, "FOREIGN_A", None),
                    (0x0101, "FOREIGN_B", Some("/dev/sg-target")),
                ],
            )],
            warnings: Vec::new(),
        };
        let mut index = CatalogIndex::open(&index_path).expect("open catalog");
        observe_drive_catalog_from_libraries(&mut index, report.libraries.iter(), &HashSet::new())
            .expect("observe foreign rows");
        let other_drive = index
            .get_actionable_drive_at("d2lib", 0x0100)
            .expect("lookup other bay")
            .expect("other bay drive");
        let target_drive = index
            .get_actionable_drive_at("d2lib", 0x0101)
            .expect("lookup target bay")
            .expect("target bay drive");
        assert_eq!(other_drive.serial, "FOREIGN_A");
        assert_eq!(target_drive.serial, "FOREIGN_B");
        drop(index);

        let drives_cfg = remanence_state::DrivesConfig {
            foreign_tapealert: true,
            ..remanence_state::DrivesConfig::default()
        };
        let mut reads = Vec::new();
        poll_foreign_drive_counters_once_with_reader(
            &index_path,
            &report,
            &drives_cfg,
            &HashSet::new(),
            |path, foreign_tapealert| {
                assert!(foreign_tapealert, "test config must request TapeAlert");
                reads.push(path.to_path_buf());
                Ok(foreign_counter_snapshot(Some("[20]")))
            },
        )
        .expect("poll foreign counters");

        assert_eq!(reads, vec![PathBuf::from("/dev/sg-target")]);
        let index = CatalogIndex::open(&index_path).expect("reopen catalog");
        assert!(
            index
                .list_drive_health_snapshots(&other_drive.drive_uuid)
                .expect("other snapshots")
                .is_empty(),
            "non-polled bay must not receive target snapshot"
        );
        let target_snapshots = index
            .list_drive_health_snapshots(&target_drive.drive_uuid)
            .expect("target snapshots");
        assert_eq!(target_snapshots.len(), 1);
        assert_eq!(target_snapshots[0].trigger, "foreign-counter");
        assert_eq!(target_snapshots[0].write_errors_corrected, Some(11));
        let advisory = index
            .list_alarms(false)
            .expect("list active alarms")
            .into_iter()
            .find(|alarm| alarm.kind == "foreign-drive-wants-cleaning")
            .expect("foreign advisory alarm");
        assert!(advisory
            .detail
            .as_deref()
            .is_some_and(|detail| { detail.contains("d2lib") && detail.contains("FOREIGN_B") }));
    }

    fn pool_state_with_tapes(tape_uuids: &[[u8; 16]]) -> ApiState {
        let mut index = test_index();
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "camera.copy-a".to_string(),
                display_name: Some("Camera copy A".to_string()),
                copy_class: Some("copy-a".to_string()),
                content_class: Some("camera".to_string()),
                created_at_utc: Some("2026-05-28T09:00:00Z".to_string()),
            })
            .expect("project tape pool");
        for tape_uuid in tape_uuids {
            project_eligible_tape(&mut index, "camera.copy-a", *tape_uuid);
        }
        ApiState::new_with_pool_configs(index, vec![pool_config("camera.copy-a")])
    }

    fn state_with_operation() -> ApiState {
        let mut index = test_index();
        let operation_id = operation_uuid();
        let session_id = Uuid::from_u128(0x33);
        for record in [
            audit_record(
                1,
                AuditEvent::OperationStarted,
                operation_id,
                Some(session_id),
                detail(&[(
                    "operation_kind",
                    CborValue::Text("write_object".to_string()),
                )]),
            ),
            audit_record(
                2,
                AuditEvent::OperationFinished,
                operation_id,
                Some(session_id),
                detail(&[("response_fingerprint", CborValue::Bytes(vec![1, 2, 3, 4]))]),
            ),
        ] {
            index
                .project_audit_record(&record)
                .expect("project operation audit record");
        }
        ApiState::new(index)
    }

    fn state_with_queued_operation() -> ApiState {
        let mut index = test_index();
        let operation_id = operation_uuid();
        let record = audit_record(
            1,
            AuditEvent::RequestReceived,
            operation_id,
            None,
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
            .expect("project queued operation");
        ApiState::new(index)
    }

    #[cfg(feature = "foreign-bru")]
    fn foreign_bru_state() -> (ApiState, String, String) {
        foreign_bru_state_with_gap(false)
    }

    #[cfg(feature = "foreign-bru")]
    fn foreign_bru_state_with_gap(include_gap: bool) -> (ApiState, String, String) {
        let mut index = test_index();
        let dump_path = write_bru_dump(include_gap);
        let source_id = dump_path.to_string_lossy().to_string();
        let unit_id = index
            .upsert_foreign_archive_projection(ForeignArchiveProjectionInput {
                tape_uuid: Vec::new(),
                format_id: "remanence-bru".to_string(),
                scan_id: "scan-bru-1".to_string(),
                source_kind: "byte_stream_dump".to_string(),
                source_id: source_id.clone(),
                confidence: "high".to_string(),
                entry_count: 1,
                damage_event_count: if include_gap { 1 } else { 0 },
                adapter_state: vec![0x42],
                last_scan_at_utc: Some("2026-05-28T13:15:00Z".to_string()),
                created_at_utc: Some("2026-05-28T13:15:01Z".to_string()),
            })
            .expect("project foreign BRU unit");
        (ApiState::new(index), unit_id, source_id)
    }

    #[cfg(feature = "foreign-bru")]
    fn write_bru_dump(include_gap: bool) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("remanence-api-bru-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create BRU fixture dir");
        let path = dir.join("fixture.bru");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&archive_block());
        if include_gap {
            bytes.extend_from_slice(&unrecognized_block());
        }
        bytes.extend_from_slice(&file_header_block("camera/a.txt", 3, b"abc"));
        std::fs::write(&path, bytes).expect("write BRU fixture");
        path
    }

    #[cfg(feature = "foreign-bru")]
    fn put_ascii(block: &mut [u8; BRU_BLOCK_SIZE], offset: usize, text: &str) {
        block[offset..offset + text.len()].copy_from_slice(text.as_bytes());
    }

    #[cfg(feature = "foreign-bru")]
    fn put_hex(block: &mut [u8; BRU_BLOCK_SIZE], offset: usize, size: usize, value: u64) {
        put_ascii(block, offset, &format!("{value:0size$x}"));
    }

    #[cfg(feature = "foreign-bru")]
    fn finalize_block(mut block: [u8; BRU_BLOCK_SIZE]) -> [u8; BRU_BLOCK_SIZE] {
        let checksum = bru_checksum(&block);
        put_ascii(&mut block, CHKSUM_OFFSET, &format!("{checksum:08x}"));
        block
    }

    #[cfg(feature = "foreign-bru")]
    fn archive_block() -> [u8; BRU_BLOCK_SIZE] {
        let mut block = [0; BRU_BLOCK_SIZE];
        put_hex(&mut block, MAGIC_OFFSET, MAGIC_SIZE, MAGIC_ARCHIVE_HEADER);
        put_hex(&mut block, ARTIME_OFFSET, 8, 0x4DE47D26);
        put_hex(&mut block, BUFSIZE_OFFSET, 8, 1024 * 1024);
        put_hex(&mut block, RELEASE_MINOR_OFFSET, 4, 17);
        put_hex(&mut block, RELEASE_MAJOR_OFFSET, 4, 1);
        put_hex(&mut block, VARIANT_OFFSET, 4, 0);
        put_hex(&mut block, ARCHIVE_ID_LOW_OFFSET, 4, 0x61A8);
        put_ascii(&mut block, LABEL_OFFSET, "TEST");
        finalize_block(block)
    }

    #[cfg(feature = "foreign-bru")]
    fn unrecognized_block() -> [u8; BRU_BLOCK_SIZE] {
        let mut block = [0; BRU_BLOCK_SIZE];
        put_hex(&mut block, MAGIC_OFFSET, MAGIC_SIZE, 0x9999);
        finalize_block(block)
    }

    #[cfg(feature = "foreign-bru")]
    fn file_header_block(path: &str, size: u64, inline: &[u8]) -> [u8; BRU_BLOCK_SIZE] {
        let mut block = [0; BRU_BLOCK_SIZE];
        put_ascii(&mut block, FILE_PATH_OFFSET, path);
        put_hex(&mut block, MAGIC_OFFSET, MAGIC_SIZE, MAGIC_FILE_HEADER);
        put_hex(&mut block, INLINE_DATA_LEN_OFFSET, 4, inline.len() as u64);
        put_hex(&mut block, ST_MODE_OFFSET, 8, S_IFREG | 0o644);
        put_hex(&mut block, ST_SIZE_OFFSET, 8, size);
        block[INLINE_DATA_OFFSET..INLINE_DATA_OFFSET + inline.len()].copy_from_slice(inline);
        finalize_block(block)
    }

    fn audit_record(
        sequence: u64,
        event: AuditEvent,
        operation_id: Uuid,
        session_id: Option<Uuid>,
        detail: BTreeMap<String, CborValue>,
    ) -> AuditRecord {
        AuditRecord {
            schema_version: 1,
            record_uuid: Uuid::from_u128(sequence as u128),
            sequence,
            timestamp_utc: format!("2026-05-28T13:15:0{sequence}Z"),
            host_id: "host".to_string(),
            process_id: 123,
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: Some(operation_id),
            session_id,
            idempotency_key: None,
            event,
            subject: AuditSubject {
                kind: "object".to_string(),
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

    fn sha256_bytes(bytes: &[u8]) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.finalize().to_vec()
    }

    /// Test sink that injects a deterministic write failure after a fixed
    /// number of successful tape blocks while preserving all captured writes.
    #[derive(Debug)]
    struct FailAfterBlocksSink {
        inner: VecBlockSink,
        max_successful_blocks: usize,
    }

    impl FailAfterBlocksSink {
        fn new(max_successful_blocks: usize) -> Self {
            Self {
                inner: VecBlockSink::new(),
                max_successful_blocks,
            }
        }
    }

    impl BlockSink for FailAfterBlocksSink {
        fn write_block(&mut self, buf: &[u8]) -> Result<WriteOutcome, TapeIoError> {
            if self.inner.blocks.len() >= self.max_successful_blocks {
                return Err(TapeIoError::OperationFailed(
                    "injected write_block failure".to_string(),
                ));
            }
            self.inner.write_block(buf)
        }

        fn write_filemarks(&mut self, count: u32) -> Result<WriteFilemarksOutcome, TapeIoError> {
            self.inner.write_filemarks(count)
        }

        fn position(&mut self) -> Result<TapePosition, TapeIoError> {
            self.inner.position()
        }
    }

    fn assert_no_pool_write_catalog_reference(
        index: &CatalogIndex,
        caller_object_id: &str,
        tape_uuid: [u8; 16],
    ) {
        assert!(
            index
                .get_native_object_by_caller_object_id(caller_object_id)
                .expect("query caller object id")
                .is_none(),
            "failed write must not leave an object row"
        );
        assert!(
            index
                .list_native_objects()
                .expect("list native objects")
                .is_empty(),
            "failed write must not leave any native object rows"
        );
        assert!(
            index
                .list_tape_files(&tape_uuid)
                .expect("list tape files")
                .is_empty(),
            "failed write must not leave committed tape-file rows"
        );
    }

    #[test]
    fn select_tape_in_pool_returns_unique_eligible_tape() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);

        let cfg = pool_config("camera.copy-a");
        let selected =
            select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select tape");

        assert_eq!(selected.pool_id, "camera.copy-a");
        assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
        assert_eq!(selected.block_size, API_SESSION_BLOCK_SIZE);
        match selected.parity_config {
            ParityConfig::Scheme(ref scheme) => assert_eq!(scheme, &test_scheme()),
            ParityConfig::None => panic!("expected parity scheme"),
        }
    }

    #[test]
    fn select_tape_in_pool_accepts_no_parity_tape_geometry() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);

        let cfg = pool_config("camera.copy-a");
        let selected =
            select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select tape");

        assert_eq!(selected.pool_id, "camera.copy-a");
        assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
        assert_eq!(selected.block_size, API_SESSION_BLOCK_SIZE);
        assert!(matches!(selected.parity_config, ParityConfig::None));
    }

    #[test]
    fn select_tape_in_pool_skips_written_no_parity_tape() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
        project_no_parity_tape(&mut index, "camera.copy-a", SECOND_POOL_WRITE_TAPE_UUID);
        project_no_parity_tape_usage(&mut index, POOL_WRITE_TAPE_UUID, 7);

        let cfg = pool_config("camera.copy-a");
        let selected =
            select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select empty tape");

        assert_eq!(selected.pool_id, "camera.copy-a");
        assert_eq!(selected.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
        assert_eq!(selected.block_size, API_SESSION_BLOCK_SIZE);
        assert!(matches!(selected.parity_config, ParityConfig::None));
    }

    #[test]
    fn select_tape_in_pool_reports_unknown_pool() {
        let index = test_index();

        let cfg = pool_config("missing.pool");
        let err =
            select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect_err("unknown pool");

        assert!(matches!(
            err,
            SelectTapeError::UnknownPool { ref pool_id } if pool_id == "missing.pool"
        ));
    }

    #[test]
    fn select_tape_in_pool_reports_empty_pool() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");

        let cfg = pool_config("camera.copy-a");
        let err = select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect_err("empty pool");

        assert!(matches!(
            err,
            SelectTapeError::EmptyPool { ref pool_id } if pool_id == "camera.copy-a"
        ));
    }

    #[test]
    fn select_tape_in_pool_uses_policy_for_multiple_eligible_tapes() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
        project_eligible_tape(&mut index, "camera.copy-a", SECOND_POOL_WRITE_TAPE_UUID);

        let cfg = pool_config("camera.copy-a");
        let selected =
            select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select tape");

        assert_eq!(selected.pool_id, "camera.copy-a");
        assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
    }

    #[test]
    fn select_tape_in_pool_excludes_reserved_preferred_tape() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
        project_eligible_tape(&mut index, "camera.copy-a", SECOND_POOL_WRITE_TAPE_UUID);
        let reserved = [POOL_WRITE_TAPE_UUID].into_iter().collect();

        let cfg = pool_config("camera.copy-a");
        let selected =
            select_tape_in_pool(&index, &cfg, 123, &reserved).expect("select unreserved tape");

        assert_eq!(selected.pool_id, "camera.copy-a");
        assert_eq!(selected.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
    }

    #[test]
    fn select_tape_in_pool_errors_when_only_eligible_tape_is_reserved() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
        let reserved = [POOL_WRITE_TAPE_UUID].into_iter().collect();

        let cfg = pool_config("camera.copy-a");
        let err = select_tape_in_pool(&index, &cfg, 123, &reserved)
            .expect_err("reserved-only pool must fail");

        assert!(matches!(
            err,
            SelectTapeError::NoUnreservedWritableTapes {
                ref pool_id,
                reserved_tape_count: 1,
            } if pool_id == "camera.copy-a"
        ));
    }

    #[test]
    fn select_tape_in_pool_skips_partially_written_tape_until_append_resume_exists() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_eligible_tape_with_voltag(
            &mut index,
            "camera.copy-a",
            POOL_WRITE_TAPE_UUID,
            "RMN004L1",
        );
        project_eligible_tape_with_voltag(
            &mut index,
            "camera.copy-a",
            SECOND_POOL_WRITE_TAPE_UUID,
            "RMN005L1",
        );
        let cfg = pool_config_with_watermarks("camera.copy-a", 0.0001, 0.0002, 0);
        let low_bytes = watermark_floor_bytes(raw_capacity_bytes(LtoGen::Lto1), cfg.watermark_low)
            .expect("low watermark");
        let object_size = u64::from(API_SESSION_BLOCK_SIZE) * 2;
        let ordinals_before_low = low_bytes / u64::from(API_SESSION_BLOCK_SIZE);
        project_ready_tape_usage(
            &mut index,
            SECOND_POOL_WRITE_TAPE_UUID,
            ordinals_before_low - 1,
        );

        let selected = select_tape_in_pool(&index, &cfg, object_size, &HashSet::new())
            .expect("select fresh tape");

        assert_eq!(selected.tape_uuid, POOL_WRITE_TAPE_UUID);
    }

    #[test]
    fn select_tape_in_pool_does_not_complete_fresh_empty_tape() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_eligible_tape_with_voltag(
            &mut index,
            "camera.copy-a",
            POOL_WRITE_TAPE_UUID,
            "RMN004L1",
        );
        project_eligible_tape_with_voltag(
            &mut index,
            "camera.copy-a",
            SECOND_POOL_WRITE_TAPE_UUID,
            "RMN005L1",
        );
        let cfg = pool_config_with_watermarks("camera.copy-a", 0.0001, 0.0002, 0);
        let low_bytes = watermark_floor_bytes(raw_capacity_bytes(LtoGen::Lto1), cfg.watermark_low)
            .expect("low watermark");
        let object_size = u64::from(API_SESSION_BLOCK_SIZE) * 2;
        let ordinals_before_low = low_bytes / u64::from(API_SESSION_BLOCK_SIZE);
        project_ready_tape_usage(&mut index, POOL_WRITE_TAPE_UUID, ordinals_before_low - 3);

        let selected = select_tape_in_pool(&index, &cfg, object_size, &HashSet::new())
            .expect("select empty tape while append resume is unwired");

        assert_eq!(selected.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
    }

    #[test]
    fn select_tape_in_pool_enforces_capacity_invariant_against_lto_capacity() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_eligible_tape_with_voltag(
            &mut index,
            "camera.copy-a",
            POOL_WRITE_TAPE_UUID,
            "RMN004L1",
        );
        let cfg = pool_config_with_watermarks("camera.copy-a", 0.10, 0.11, 1_000_000_001);

        let err = select_tape_in_pool(&index, &cfg, 1, &HashSet::new())
            .expect_err("too-narrow watermark band must reject");

        assert!(
            matches!(
                err,
                SelectTapeError::State(StateError::ConfigInvalid(ref message))
                    if message.contains("watermark band")
            ),
            "{err}"
        );
    }

    #[test]
    fn seal_decision_uses_actual_position_inclusive_boundary() {
        assert_eq!(
            seal_decision_after_write(
                TapePositionAfterWrite {
                    used_bytes: 100,
                    early_warning: false,
                },
                100,
                None,
            ),
            Some(TapeSealReason::ReachedLowWatermark)
        );
    }

    #[test]
    fn seal_decision_keeps_below_low_tape_active_without_force_or_early_warning() {
        assert_eq!(
            seal_decision_after_write(
                TapePositionAfterWrite {
                    used_bytes: 99,
                    early_warning: false,
                },
                100,
                None,
            ),
            None
        );
    }

    #[test]
    fn seal_decision_hardware_early_warning_wins_below_low() {
        assert_eq!(
            seal_decision_after_write(
                TapePositionAfterWrite {
                    used_bytes: 10,
                    early_warning: true,
                },
                100,
                None,
            ),
            Some(TapeSealReason::HardwareEarlyWarning)
        );
    }

    #[test]
    fn seal_decision_honors_force_seal_valve_below_low() {
        assert_eq!(
            seal_decision_after_write(
                TapePositionAfterWrite {
                    used_bytes: 10,
                    early_warning: false,
                },
                100,
                Some(TapeSealReason::NoPendingObjectFits),
            ),
            Some(TapeSealReason::NoPendingObjectFits)
        );
    }

    #[test]
    fn write_object_to_pool_returns_locator_commits_catalog_and_round_trips_payload() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_eligible_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
        let source_dir = temp_dir("remanence-api-pool-write-src");
        let restore_dir = temp_dir("remanence-api-pool-write-restore");
        let source_path = source_dir.join("payload.bin");
        let payload = b"pool targeted write payload".to_vec();
        std::fs::write(&source_path, &payload).expect("write source payload");
        let expected_hash = sha256_bytes(&payload);
        let mut tape_sink = VecBlockSink::new();
        let cfg = pool_config("camera.copy-a");

        let result = write_object_to_pool(
            &mut index,
            &mut tape_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: " camera.copy-a ".to_string(),
                source_path: source_path.clone(),
                archive_path: "payload.bin".into(),
                caller_object_id: "caller-pool-core".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
        )
        .expect("write object to pool");

        assert_eq!(result.object.caller_object_id, "caller-pool-core");
        assert_eq!(result.object.content_sha256.to_vec(), expected_hash);
        assert_eq!(result.object.logical_size_bytes, payload.len() as u64);
        assert_eq!(result.object.body_format, "rao-v1");
        assert_eq!(result.object.copies.len(), 1);
        let copy = &result.object.copies[0];
        assert_eq!(copy.tape_uuid, POOL_WRITE_TAPE_UUID);
        assert_eq!(copy.pool_id, "camera.copy-a");
        assert_eq!(
            copy.tape_file_number,
            u64::from(result.write_report.object_close.tape_file_number)
        );
        assert_eq!(
            copy.first_body_lba,
            result.write_report.catalog.files[0]
                .first_chunk_lba
                .expect("payload lba")
                .0
        );

        let committed = index
            .get_native_object(result.object.object_id_text().as_str())
            .expect("query committed object")
            .expect("committed object exists");
        assert_eq!(
            committed.caller_object_id.as_deref(),
            Some("caller-pool-core")
        );
        assert_eq!(
            committed.content_hash.as_deref(),
            Some(expected_hash.as_slice())
        );
        assert_eq!(committed.copies.len(), 1);
        assert_eq!(
            committed.copies[0].pool_id.as_deref(),
            Some(copy.pool_id.as_str())
        );
        assert_eq!(committed.copies[0].first_body_lba, copy.first_body_lba);
        let projected_file = &result.write_report.catalog.files[0];
        let committed_file = index
            .get_native_object_file(
                result.object.object_id_text().as_str(),
                projected_file.file_id.as_str(),
            )
            .expect("query committed object file")
            .expect("committed object file exists");
        assert_eq!(committed_file.path, "payload.bin");
        assert_eq!(
            committed_file.size_bytes,
            u64::try_from(payload.len()).expect("payload length fits u64")
        );
        assert_eq!(committed_file.file_sha256, expected_hash);
        assert_eq!(
            committed_file.first_chunk_lba,
            projected_file.first_chunk_lba.map(|lba| lba.0)
        );
        assert_eq!(committed_file.chunk_count, projected_file.chunk_count);

        let object_block_start = 1usize;
        let object_block_count = usize::try_from(result.write_report.object_close.data_block_count)
            .expect("object block count fits usize");
        let object_blocks =
            tape_sink.blocks[object_block_start..object_block_start + object_block_count].to_vec();
        let mut object_source = VecBlockSource::new(object_blocks);
        let restore = restore_object_to_directory(
            &mut object_source,
            API_SESSION_BLOCK_SIZE as usize,
            result.write_report.layout.projected_size_blocks,
            &restore_dir,
            FilesystemRestoreOptions::default(),
        )
        .expect("restore object");

        assert_eq!(restore.files_written, 1);
        assert_eq!(
            std::fs::read(restore_dir.join("payload.bin")).unwrap(),
            payload
        );
    }

    #[test]
    fn no_parity_write_round_trips_payload_and_commits_without_parity_geometry() {
        let mut index = test_index();
        project_pool(&mut index, "scenario-a");
        project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
        let source_dir = temp_dir("remanence-api-no-parity-src");
        let source_path = source_dir.join("payload.bin");
        let payload = b"scenario-a no parity payload".to_vec();
        std::fs::write(&source_path, &payload).expect("write source payload");
        let mut tape_sink = VecBlockSink::new();
        let cfg = pool_config("scenario-a");

        let result = write_object_to_pool(
            &mut index,
            &mut tape_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path,
                archive_path: "payload.bin".into(),
                caller_object_id: "caller-no-parity".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
        )
        .expect("write no-parity object");

        assert_eq!(tape_sink.filemarks, vec![1, 1]);
        assert_eq!(
            result
                .write_report
                .catalog
                .object_copy
                .first_parity_data_ordinal,
            None
        );
        assert_eq!(
            result
                .write_report
                .catalog
                .object_copy
                .protected_until_ordinal,
            None
        );
        let bootstrap =
            parse_bootstrap_block(&tape_sink.blocks[0]).expect("parse no-parity bootstrap");
        assert!(bootstrap.no_parity_flag);
        assert!(bootstrap.scheme.is_none());
        assert_eq!(bootstrap.tape_uuid, POOL_WRITE_TAPE_UUID);

        let mut source = VecBlockSource::new(tape_sink.blocks.clone());
        verify_tape_identity(&mut source, &POOL_WRITE_TAPE_UUID)
            .expect("verify matching no-parity bootstrap");
        let read = read_rem_tar_object(
            &mut source,
            API_SESSION_BLOCK_SIZE as usize,
            result.write_report.layout.projected_size_blocks,
        )
        .expect("read no-parity RAO object");
        assert_eq!(
            read.entry("payload.bin").expect("payload entry").data,
            payload
        );

        let tape = index
            .get_tape(&POOL_WRITE_TAPE_UUID)
            .expect("query tape")
            .expect("tape row");
        assert_eq!(tape.scheme_id, None);
        assert_eq!(tape.data_blocks_per_stripe, None);
        assert_eq!(tape.parity_blocks_per_stripe, None);
        assert_eq!(tape.stripes_per_neighborhood, None);

        let committed = index
            .get_native_object(result.object.object_id_text().as_str())
            .expect("query committed no-parity object")
            .expect("committed no-parity object exists");
        assert_eq!(
            committed.metadata_hash.as_deref(),
            Some(&result.write_report.catalog.object.manifest_sha256[..])
        );
        assert_eq!(committed.copies.len(), 1);
        assert_eq!(committed.copies[0].first_parity_data_ordinal, None);
        assert_eq!(committed.copies[0].protected_until_ordinal, None);
    }

    #[test]
    fn encrypted_no_parity_write_round_trips_payload_and_commits_envelope_fields() {
        let mut index = test_index();
        project_pool(&mut index, "scenario-a");
        project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
        let source_dir = temp_dir("remanence-api-encrypted-no-parity-src");
        let source_path = source_dir.join("payload.bin");
        let payload = b"encrypted no parity payload".to_vec();
        std::fs::write(&source_path, &payload).expect("write source payload");
        let mut tape_sink = VecBlockSink::new();
        let cfg = pool_config("scenario-a");
        let root_key = RootKey::new([0x31; 32]).expect("root key");
        let key_id = [0x42; 16];

        let result = write_object_to_pool(
            &mut index,
            &mut tape_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path,
                archive_path: "payload.bin".into(),
                caller_object_id: "caller-encrypted-no-parity".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Encrypted {
                    root_key: root_key.clone(),
                    key_id,
                },
            },
        )
        .expect("write encrypted no-parity object");

        assert_eq!(tape_sink.filemarks, vec![1, 1]);
        assert_eq!(
            result.object.copies[0].representation,
            OBJECT_COPY_REPRESENTATION_ENCRYPTED
        );
        assert_eq!(result.object.copies[0].key_id, Some(key_id));
        let metadata_frame_len = result.object.copies[0]
            .metadata_frame_len
            .expect("metadata frame length");
        let bootstrap_row = result
            .write_report
            .object_close
            .bootstrap_object_row
            .as_ref()
            .expect("encrypted bootstrap row");
        match &bootstrap_row.representation {
            remanence_parity::bootstrap::BootstrapObjectRepresentation::Encrypted {
                key_id: row_key_id,
                metadata_frame_len: row_metadata_frame_len,
            } => {
                assert_eq!(*row_key_id, key_id);
                assert_eq!(*row_metadata_frame_len, metadata_frame_len);
            }
            other => panic!("unexpected bootstrap row representation: {other:?}"),
        }

        let committed = index
            .get_native_object(result.object.object_id_text().as_str())
            .expect("query encrypted object")
            .expect("encrypted object exists");
        assert_eq!(committed.metadata_hash, None);
        assert_eq!(committed.copies.len(), 1);
        assert_eq!(
            committed.copies[0].representation,
            OBJECT_COPY_REPRESENTATION_ENCRYPTED
        );
        assert_eq!(committed.copies[0].key_id.as_deref(), Some(&key_id[..]));
        assert_eq!(
            committed.copies[0].metadata_frame_len,
            Some(metadata_frame_len)
        );

        let stored_block_count = usize::try_from(result.write_report.object_close.data_block_count)
            .expect("stored block count fits usize");
        let mut source = VecBlockSource::new(tape_sink.blocks[1..1 + stored_block_count].to_vec());
        let opened = read_encrypted_rao_object(
            &mut source,
            API_SESSION_BLOCK_SIZE as usize,
            result.write_report.object_close.data_block_count,
            &root_key,
        )
        .expect("decrypt encrypted RAO object");
        let restored = opened.object.entry("payload.bin").expect("payload entry");
        assert_eq!(restored.data, payload);
        assert_eq!(opened.envelope.header.key_id, key_id);
        assert_eq!(
            opened.envelope.header.metadata_frame_len,
            metadata_frame_len
        );
    }

    #[test]
    fn pool_write_rejects_pool_tape_block_size_mismatch_before_tape_io() {
        let mut index = test_index();
        project_pool(&mut index, "scenario-a");
        project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
        let source_dir = temp_dir("remanence-api-block-mismatch-src");
        let source_path = source_dir.join("payload.bin");
        std::fs::write(&source_path, b"block mismatch must not reach tape")
            .expect("write source payload");
        let cfg = pool_config_with_block_size("scenario-a", API_SESSION_BLOCK_SIZE * 2);
        let mut tape_sink = VecBlockSink::new();

        let err = write_object_to_pool(
            &mut index,
            &mut tape_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path,
                archive_path: "payload.bin".into(),
                caller_object_id: "caller-block-mismatch".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
        )
        .expect_err("pool/tape block-size mismatch must reject");

        match err {
            PoolWriteError::Select(SelectTapeError::NoWritableTapes { pool_id, reasons }) => {
                assert_eq!(pool_id, "scenario-a");
                assert!(
                    reasons.iter().any(|reason| matches!(
                        reason,
                        WritabilityError::BlockSizeMismatch {
                            tape_block_size,
                            pool_block_size,
                        } if *tape_block_size == u64::from(API_SESSION_BLOCK_SIZE)
                            && *pool_block_size == u64::from(API_SESSION_BLOCK_SIZE * 2)
                    )),
                    "{reasons:?}"
                );
            }
            other => panic!("unexpected pool write error: {other}"),
        }
        assert!(tape_sink.blocks.is_empty());
        assert!(tape_sink.filemarks.is_empty());

        let selected_path = source_dir.join("selected-payload.bin");
        std::fs::write(&selected_path, b"selected mismatch must not reach tape")
            .expect("write selected source payload");
        let selected = SelectedTape {
            pool_id: "scenario-a".to_string(),
            tape_uuid: POOL_WRITE_TAPE_UUID,
            block_size: API_SESSION_BLOCK_SIZE,
            parity_config: ParityConfig::None,
        };
        let mut selected_sink = VecBlockSink::new();

        let err = write_to_selected_tape(
            &mut index,
            &mut selected_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path: selected_path,
                archive_path: "selected-payload.bin".into(),
                caller_object_id: "caller-selected-block-mismatch".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected,
        )
        .expect_err("selected pool/tape block-size mismatch must reject");

        assert!(
            matches!(
                err,
                PoolWriteError::InvalidInput(ref message)
                    if message.contains("does not match pool configured block size")
            ),
            "{err}"
        );
        assert!(selected_sink.blocks.is_empty());
        assert!(selected_sink.filemarks.is_empty());
    }

    #[test]
    fn pool_write_uses_selected_tape_block_size_as_rao_chunk_size() {
        const CUSTOM_BLOCK_SIZE: u32 = 8192;

        let mut plaintext_index = test_index();
        project_pool(&mut plaintext_index, "custom-plain");
        project_no_parity_tape_with_block_size(
            &mut plaintext_index,
            "custom-plain",
            POOL_WRITE_TAPE_UUID,
            CUSTOM_BLOCK_SIZE,
        );
        let source_dir = temp_dir("remanence-api-custom-block-plain-src");
        let plaintext_path = source_dir.join("plain.bin");
        let plaintext_payload = b"plaintext custom block size payload".to_vec();
        std::fs::write(&plaintext_path, &plaintext_payload).expect("write plaintext source");
        let mut plaintext_sink = VecBlockSink::new();
        let plaintext_cfg = pool_config_with_block_size("custom-plain", CUSTOM_BLOCK_SIZE);

        let plaintext = write_object_to_pool(
            &mut plaintext_index,
            &mut plaintext_sink,
            &plaintext_cfg,
            WriteObjectToPoolRequest {
                pool_id: "custom-plain".to_string(),
                source_path: plaintext_path,
                archive_path: "plain.bin".into(),
                caller_object_id: "caller-custom-block-plain".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
        )
        .expect("write plaintext custom-block object");

        assert_eq!(
            plaintext.write_report.layout.chunk_size,
            CUSTOM_BLOCK_SIZE as usize
        );
        assert_eq!(
            parse_bootstrap_block(&plaintext_sink.blocks[0])
                .expect("parse plaintext bootstrap")
                .block_size_bytes,
            CUSTOM_BLOCK_SIZE
        );
        assert_eq!(
            plaintext_index
                .get_tape(&POOL_WRITE_TAPE_UUID)
                .expect("query plaintext custom-block tape")
                .expect("plaintext custom-block tape exists")
                .block_size,
            Some(u64::from(CUSTOM_BLOCK_SIZE))
        );
        let plaintext_block_count =
            usize::try_from(plaintext.write_report.object_close.data_block_count)
                .expect("plaintext object block count fits usize");
        let mut plaintext_source =
            VecBlockSource::new(plaintext_sink.blocks[1..1 + plaintext_block_count].to_vec());
        let plaintext_read = read_rem_tar_object(
            &mut plaintext_source,
            CUSTOM_BLOCK_SIZE as usize,
            plaintext.write_report.object_close.data_block_count,
        )
        .expect("read plaintext custom-block RAO object");
        assert_eq!(
            plaintext_read
                .global_pax
                .get("REMANENCE.chunk_size")
                .map(String::as_str),
            Some("8192")
        );
        assert_eq!(
            plaintext_read.entry("plain.bin").expect("plain entry").data,
            plaintext_payload
        );

        let mut encrypted_index = test_index();
        project_pool(&mut encrypted_index, "custom-encrypted");
        project_no_parity_tape_with_block_size(
            &mut encrypted_index,
            "custom-encrypted",
            SECOND_POOL_WRITE_TAPE_UUID,
            CUSTOM_BLOCK_SIZE,
        );
        let source_dir = temp_dir("remanence-api-custom-block-encrypted-src");
        let encrypted_path = source_dir.join("secret.bin");
        let encrypted_payload = b"encrypted custom block size payload".to_vec();
        std::fs::write(&encrypted_path, &encrypted_payload).expect("write encrypted source");
        let mut encrypted_sink = VecBlockSink::new();
        let encrypted_cfg = pool_config_with_block_size("custom-encrypted", CUSTOM_BLOCK_SIZE);
        let root_key = RootKey::new([0x52; 32]).expect("root key");
        let key_id = [0x62; 16];

        let encrypted = write_object_to_pool(
            &mut encrypted_index,
            &mut encrypted_sink,
            &encrypted_cfg,
            WriteObjectToPoolRequest {
                pool_id: "custom-encrypted".to_string(),
                source_path: encrypted_path,
                archive_path: "secret.bin".into(),
                caller_object_id: "caller-custom-block-encrypted".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Encrypted {
                    root_key: root_key.clone(),
                    key_id,
                },
            },
        )
        .expect("write encrypted custom-block object");

        assert_eq!(
            encrypted.write_report.layout.chunk_size,
            CUSTOM_BLOCK_SIZE as usize
        );
        assert_eq!(
            parse_bootstrap_block(&encrypted_sink.blocks[0])
                .expect("parse encrypted bootstrap")
                .block_size_bytes,
            CUSTOM_BLOCK_SIZE
        );
        assert_eq!(
            encrypted_index
                .get_tape(&SECOND_POOL_WRITE_TAPE_UUID)
                .expect("query encrypted custom-block tape")
                .expect("encrypted custom-block tape exists")
                .block_size,
            Some(u64::from(CUSTOM_BLOCK_SIZE))
        );
        let encrypted_block_count =
            usize::try_from(encrypted.write_report.object_close.data_block_count)
                .expect("encrypted object block count fits usize");
        let mut encrypted_source =
            VecBlockSource::new(encrypted_sink.blocks[1..1 + encrypted_block_count].to_vec());
        let opened = read_encrypted_rao_object(
            &mut encrypted_source,
            CUSTOM_BLOCK_SIZE as usize,
            encrypted.write_report.object_close.data_block_count,
            &root_key,
        )
        .expect("decrypt encrypted custom-block RAO object");
        assert_eq!(opened.envelope.header.chunk_size, CUSTOM_BLOCK_SIZE);
        assert_eq!(
            opened
                .object
                .global_pax
                .get("REMANENCE.chunk_size")
                .map(String::as_str),
            Some("8192")
        );
        assert_eq!(
            opened
                .object
                .entry("secret.bin")
                .expect("secret entry")
                .data,
            encrypted_payload
        );
    }

    #[test]
    fn encrypted_write_transfer_failure_leaves_no_durable_catalog_reference() {
        let mut index = test_index();
        project_pool(&mut index, "scenario-a");
        project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
        let source_dir = temp_dir("remanence-api-encrypted-transfer-fail-src");
        let source_path = source_dir.join("payload.bin");
        let payload = b"encrypted transfer failure payload".to_vec();
        std::fs::write(&source_path, &payload).expect("write source payload");
        let cfg = pool_config("scenario-a");
        let selected = select_tape_in_pool(&index, &cfg, payload.len() as u64, &HashSet::new())
            .expect("select no-parity tape");
        let root_key = RootKey::new([0x31; 32]).expect("root key");
        let key_id = [0x42; 16];
        let mut tape_sink = FailAfterBlocksSink::new(1);

        let err = write_to_selected_tape(
            &mut index,
            &mut tape_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path,
                archive_path: "payload.bin".into(),
                caller_object_id: "caller-encrypted-transfer-fail".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Encrypted { root_key, key_id },
            },
            selected,
        )
        .expect_err("injected transfer error must fail the write");

        assert!(
            matches!(
                err,
                PoolWriteError::TapeIo(TapeIoError::OperationFailed(ref message))
                    if message.contains("injected write_block failure")
            ),
            "{err}"
        );
        assert_eq!(
            tape_sink.inner.blocks.len(),
            1,
            "only the tape bootstrap should be written before the injected failure"
        );
        assert_eq!(
            tape_sink.inner.filemarks,
            vec![1],
            "failed transfer must not write an object-closing filemark"
        );
        assert_no_pool_write_catalog_reference(
            &index,
            "caller-encrypted-transfer-fail",
            POOL_WRITE_TAPE_UUID,
        );
    }

    #[test]
    fn plaintext_write_transfer_failure_leaves_no_durable_catalog_reference() {
        let mut index = test_index();
        project_pool(&mut index, "scenario-a");
        project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
        let source_dir = temp_dir("remanence-api-plaintext-transfer-fail-src");
        let source_path = source_dir.join("payload.bin");
        let payload = b"plaintext transfer failure payload".to_vec();
        std::fs::write(&source_path, &payload).expect("write source payload");
        let cfg = pool_config("scenario-a");
        let selected = select_tape_in_pool(&index, &cfg, payload.len() as u64, &HashSet::new())
            .expect("select no-parity tape");
        let mut tape_sink = FailAfterBlocksSink::new(1);

        let err = write_to_selected_tape(
            &mut index,
            &mut tape_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path,
                archive_path: "payload.bin".into(),
                caller_object_id: "caller-plaintext-transfer-fail".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected,
        )
        .expect_err("injected transfer error must fail the write");

        assert!(
            err.to_string().contains("injected write_block failure"),
            "{err}"
        );
        assert_eq!(
            tape_sink.inner.blocks.len(),
            1,
            "only the tape bootstrap should be written before the injected failure"
        );
        assert_eq!(
            tape_sink.inner.filemarks,
            vec![1],
            "failed transfer must not write an object-closing filemark"
        );
        assert_no_pool_write_catalog_reference(
            &index,
            "caller-plaintext-transfer-fail",
            POOL_WRITE_TAPE_UUID,
        );
    }

    /// §10.6 integration: the recycle-concern repro with **no** catalog
    /// reset anywhere. Init → write+commit → retire → re-init the same
    /// barcode under a fresh identity → write+commit; the first identity's
    /// copies read back `missing` while the second's read back `committed`.
    #[test]
    fn retire_then_reinit_same_barcode_round_trips_without_catalog_reset() {
        const FIRST_UUID: [u8; 16] = [0xA1; 16];
        const SECOND_UUID: [u8; 16] = [0xB2; 16];
        const VOLTAG: &str = "RMN161L9";
        let mut index = test_index();
        project_pool(&mut index, "scenario-a");
        project_no_parity_tape(&mut index, "scenario-a", FIRST_UUID);
        let source_dir = temp_dir("remanence-api-retire-cycle-src");
        let first_path = source_dir.join("first.bin");
        let second_path = source_dir.join("second.bin");
        std::fs::write(&first_path, b"first lifecycle payload").expect("write first payload");
        std::fs::write(&second_path, b"second lifecycle payload").expect("write second payload");
        let cfg = pool_config("scenario-a");

        // Write + commit an object to the first identity (in-memory tape).
        let mut first_sink = VecBlockSink::new();
        let first = write_object_to_pool(
            &mut index,
            &mut first_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path: first_path,
                archive_path: "first.bin".into(),
                caller_object_id: "caller-retire-first".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
        )
        .expect("first write succeeds");
        assert_eq!(first.object.copies[0].tape_uuid, FIRST_UUID);

        // Retire the identity — catalog + audit only, no hardware.
        let outcome = index
            .retire_tape(remanence_state::RetireTapeInput {
                tape_uuid: FIRST_UUID,
                reason: "recycled".to_string(),
            })
            .expect("retire first identity");
        assert!(outcome.newly_retired);
        assert_eq!(outcome.released_voltag.as_deref(), Some(VOLTAG));
        assert_eq!(outcome.copies_marked_missing, 1);

        // Re-init the same physical medium: BOT still carries the retired
        // identity's bootstrap with object data past it.
        let mut bot_source = VecBlockSource::new(first_sink.blocks.clone());
        let projection = classify_bot_from_source(&mut bot_source);
        assert!(
            projection.physical_data_past_bootstrap,
            "fixture must reproduce the concern doc's data-past-bootstrap state"
        );
        let catalog_inputs = project_tape_init_catalog_inputs(
            &index,
            VOLTAG,
            &projection.classification,
            "scenario-a",
        )
        .expect("project init inputs");
        assert_eq!(
            catalog_inputs.barcode_state,
            BarcodeLifecycleState::Available
        );
        assert_eq!(
            catalog_inputs
                .catalog_row
                .as_ref()
                .map(|row| row.disposition),
            Some(CatalogRowDisposition::Retired)
        );
        let decision = decide_tape_init(
            &projection.classification,
            catalog_inputs.catalog_row.as_ref(),
            &catalog_inputs.barcode_state,
            "scenario-a",
            projection.physical_data_past_bootstrap,
            &catalog_inputs.committed_copies,
        );
        assert_eq!(
            decision,
            InitDecision::FreshInit,
            "retired identity must re-init without CLOBBER or force"
        );

        // Fresh bootstrap + fresh catalog row for the same barcode.
        let mut reinit_sink = VecBlockSink::new();
        let action = maybe_write_tape_init_bootstrap(
            &mut reinit_sink,
            &decision,
            TapeInitWriteOptions::default(),
            SECOND_UUID,
            API_SESSION_BLOCK_SIZE,
            ParityConfig::None,
            "test",
        )
        .expect("write fresh bootstrap");
        assert_eq!(action, TapeInitWriteAction::WroteBootstrap);
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid: SECOND_UUID,
                voltag: VOLTAG.to_string(),
                block_size: API_SESSION_BLOCK_SIZE,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision fresh identity for the released barcode");
        index
            .project_tape_pool_membership(SECOND_UUID, "scenario-a")
            .expect("assign fresh identity to pool");

        // Write + commit a second object: selection must pick the fresh
        // identity (the retired one is not `ready`).
        let mut second_sink = VecBlockSink::new();
        let second = write_object_to_pool(
            &mut index,
            &mut second_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path: second_path,
                archive_path: "second.bin".into(),
                caller_object_id: "caller-retire-second".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
        )
        .expect("second write succeeds without any catalog reset");
        assert_eq!(second.object.copies[0].tape_uuid, SECOND_UUID);

        let first_record = index
            .get_native_object(first.object.object_id_text().as_str())
            .expect("query first object")
            .expect("first object exists");
        assert_eq!(first_record.copies.len(), 1);
        assert_eq!(first_record.copies[0].status, "missing");
        let second_record = index
            .get_native_object(second.object.object_id_text().as_str())
            .expect("query second object")
            .expect("second object exists");
        assert_eq!(second_record.copies.len(), 1);
        assert_eq!(second_record.copies[0].status, "committed");
        assert_eq!(
            index
                .list_objects_with_no_committed_copies()
                .expect("degraded objects"),
            vec![first.object.object_id_text()]
        );
    }

    #[test]
    fn write_to_selected_tape_rejects_second_no_parity_object_before_tape_io() {
        let mut index = test_index();
        project_pool(&mut index, "scenario-a");
        project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
        let source_dir = temp_dir("remanence-api-no-parity-reuse-src");
        let first_path = source_dir.join("first.bin");
        let second_path = source_dir.join("second.bin");
        std::fs::write(&first_path, b"first no parity payload").expect("write first payload");
        std::fs::write(&second_path, b"second no parity payload").expect("write second payload");
        let cfg = pool_config("scenario-a");
        let selected =
            select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select no-parity tape");
        let mut first_sink = VecBlockSink::new();

        let first = write_to_selected_tape(
            &mut index,
            &mut first_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path: first_path,
                archive_path: "first.bin".into(),
                caller_object_id: "caller-no-parity-first".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected.clone(),
        )
        .expect("first no-parity write succeeds");
        let mut second_sink = VecBlockSink::new();

        let err = write_to_selected_tape(
            &mut index,
            &mut second_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path: second_path,
                archive_path: "second.bin".into(),
                caller_object_id: "caller-no-parity-second".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected,
        )
        .expect_err("second no-parity write must reject");

        assert!(
            matches!(
                err,
                PoolWriteError::NoParityAppendUnsupported {
                    ref tape_uuid,
                    total_committed_ordinals
                } if tape_uuid == &Uuid::from_bytes(POOL_WRITE_TAPE_UUID).to_string()
                    && total_committed_ordinals == first.write_report.layout.projected_size_blocks
            ),
            "{err}"
        );
        assert!(second_sink.blocks.is_empty());
        assert!(second_sink.filemarks.is_empty());
    }

    #[test]
    fn write_to_selected_tape_rejects_second_parity_object_before_tape_io() {
        let mut index = test_index();
        project_pool(&mut index, "scenario-a");
        project_eligible_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
        let source_dir = temp_dir("remanence-api-parity-reuse-src");
        let first_path = source_dir.join("first.bin");
        let second_path = source_dir.join("second.bin");
        std::fs::write(&first_path, b"first parity payload").expect("write first payload");
        std::fs::write(&second_path, b"second parity payload").expect("write second payload");
        let cfg = pool_config("scenario-a");
        let selected =
            select_tape_in_pool(&index, &cfg, 123, &HashSet::new()).expect("select parity tape");
        let mut first_sink = VecBlockSink::new();

        let first = write_to_selected_tape(
            &mut index,
            &mut first_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path: first_path,
                archive_path: "first.bin".into(),
                caller_object_id: "caller-parity-first".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected.clone(),
        )
        .expect("first parity write succeeds");
        let mut second_sink = VecBlockSink::new();

        let err = write_to_selected_tape(
            &mut index,
            &mut second_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path: second_path,
                archive_path: "second.bin".into(),
                caller_object_id: "caller-parity-second".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected,
        )
        .expect_err("second parity write must reject before opening at BOT");

        assert!(
            matches!(
                err,
                PoolWriteError::ParityAppendUnsupported {
                    ref tape_uuid,
                    total_committed_ordinals
                } if tape_uuid == &Uuid::from_bytes(POOL_WRITE_TAPE_UUID).to_string()
                    && total_committed_ordinals
                        == first.write_report.catalog.tape_file_bundle.total_committed_ordinals
            ),
            "{err}"
        );
        assert!(second_sink.blocks.is_empty());
        assert!(second_sink.filemarks.is_empty());
    }

    #[test]
    fn write_rejects_expected_content_sha256_mismatch_before_writing() {
        let mut index = test_index();
        project_pool(&mut index, "scenario-a");
        project_no_parity_tape(&mut index, "scenario-a", POOL_WRITE_TAPE_UUID);
        let source_dir = temp_dir("remanence-api-hash-mismatch-src");
        let source_path = source_dir.join("payload.bin");
        let payload = b"hash mismatch must stop before tape I/O".to_vec();
        std::fs::write(&source_path, &payload).expect("write source payload");
        let cfg = pool_config("scenario-a");
        let selected = select_tape_in_pool(&index, &cfg, payload.len() as u64, &HashSet::new())
            .expect("select tape");
        let mut tape_sink = VecBlockSink::new();
        let mut wrong_hash = [0u8; 32];
        wrong_hash[0] = 1;

        let err = write_to_selected_tape(
            &mut index,
            &mut tape_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "scenario-a".to_string(),
                source_path,
                archive_path: "payload.bin".into(),
                caller_object_id: "caller-hash-mismatch".to_string(),
                expected_content_sha256: Some(wrong_hash),
                representation: PoolWriteRepresentation::Plaintext,
            },
            selected,
        )
        .expect_err("hash mismatch must reject before tape write");

        assert!(
            matches!(err, PoolWriteError::ContentHashMismatch { .. }),
            "{err}"
        );
        assert!(tape_sink.blocks.is_empty());
        assert!(tape_sink.filemarks.is_empty());
    }

    #[test]
    fn write_object_to_pool_seals_after_crossing_low_and_excludes_tape() {
        let mut index = test_index();
        project_pool(&mut index, "seal.pool");
        project_no_parity_tape(&mut index, "seal.pool", POOL_WRITE_TAPE_UUID);
        project_no_parity_tape(&mut index, "seal.pool", SECOND_POOL_WRITE_TAPE_UUID);
        let cfg = pool_config_with_watermarks("seal.pool", 0.00000000001, 0.000000001, 0);
        let source_dir = temp_dir("remanence-api-seal-src");
        let source_path = source_dir.join("payload.bin");
        std::fs::write(&source_path, b"seal after actual position crosses low")
            .expect("write source payload");
        let mut tape_sink = VecBlockSink::new();

        let result = write_object_to_pool(
            &mut index,
            &mut tape_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "seal.pool".to_string(),
                source_path,
                archive_path: "payload.bin".into(),
                caller_object_id: "caller-seal".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
        )
        .expect("write and seal first tape");

        assert_eq!(result.object.copies[0].tape_uuid, POOL_WRITE_TAPE_UUID);
        let sealed = index
            .get_tape(&POOL_WRITE_TAPE_UUID)
            .expect("query sealed tape")
            .expect("sealed tape exists");
        assert_eq!(sealed.state, "sealed");

        let selected = select_tape_in_pool(&index, &cfg, 1, &HashSet::new())
            .expect("select unsealed tape after seal");
        assert_eq!(selected.tape_uuid, SECOND_POOL_WRITE_TAPE_UUID);
    }

    #[test]
    fn verify_tape_identity_accepts_match_and_rejects_mismatch_or_absent_bootstrap() {
        let mut matching =
            VecBlockSource::new(vec![no_parity_bootstrap_block(POOL_WRITE_TAPE_UUID)]);
        verify_tape_identity(&mut matching, &POOL_WRITE_TAPE_UUID).expect("matching identity");

        let mut mismatched =
            VecBlockSource::new(vec![no_parity_bootstrap_block(POOL_WRITE_TAPE_UUID)]);
        let err = verify_tape_identity(&mut mismatched, &SECOND_POOL_WRITE_TAPE_UUID)
            .expect_err("mismatched identity");
        assert!(matches!(err, TapeIdentityError::Mismatch { .. }), "{err}");

        let mut absent = VecBlockSource::new(vec![vec![0u8; API_SESSION_BLOCK_SIZE as usize]]);
        let err =
            verify_tape_identity(&mut absent, &POOL_WRITE_TAPE_UUID).expect_err("absent bootstrap");
        assert!(
            matches!(err, TapeIdentityError::AbsentBootstrap(_)),
            "{err}"
        );
    }

    #[test]
    fn write_object_to_pool_rejects_non_regular_source_as_invalid_argument() {
        let mut index = test_index();
        project_pool(&mut index, "camera.copy-a");
        project_no_parity_tape(&mut index, "camera.copy-a", POOL_WRITE_TAPE_UUID);
        let mut tape_sink = VecBlockSink::new();
        let source_dir = temp_dir("remanence-api-pool-write-dir-src");
        let cfg = pool_config("camera.copy-a");

        let err = write_object_to_pool(
            &mut index,
            &mut tape_sink,
            &cfg,
            WriteObjectToPoolRequest {
                pool_id: "camera.copy-a".to_string(),
                source_path: source_dir,
                archive_path: "payload.bin".into(),
                caller_object_id: "caller-non-regular".to_string(),
                expected_content_sha256: None,
                representation: PoolWriteRepresentation::Plaintext,
            },
        )
        .expect_err("directory source must be caller-fault input");

        assert!(
            matches!(&err, PoolWriteError::InvalidInput(message) if message.contains("not a regular file")),
            "{err}"
        );
        assert!(tape_sink.blocks.is_empty());
        assert!(tape_sink.filemarks.is_empty());
        assert_no_pool_write_catalog_reference(&index, "caller-non-regular", POOL_WRITE_TAPE_UUID);
    }

    #[tokio::test]
    async fn daemon_health_and_version_are_wired() {
        let service = populated_state().daemon_service();

        let health = pb::daemon_server::Daemon::health(&service, Request::new(()))
            .await
            .expect("health")
            .into_inner();
        assert_eq!(health.status, pb::health_response::Status::Healthy as i32);
        assert_eq!(
            health.components.get("sqlite_index").map(String::as_str),
            Some("ok")
        );

        let version = pb::daemon_server::Daemon::version(&service, Request::new(()))
            .await
            .expect("version")
            .into_inner();
        assert_eq!(version.api_version, "v1-draft");
        assert!(!version.daemon_version.is_empty());
        assert!(!version.rust_target.is_empty());
    }

    #[tokio::test]
    async fn daemon_operations_are_projected() {
        let service = state_with_operation().daemon_service();
        let operation_id = operation_uuid();

        let status = pb::daemon_server::Daemon::get_operation(
            &service,
            Request::new(pb::GetOperationRequest {
                operation_id: operation_id.as_bytes().to_vec(),
            }),
        )
        .await
        .expect("get operation")
        .into_inner();
        assert_eq!(status.operation_id, operation_id.as_bytes().to_vec());
        assert_eq!(status.operation_kind, "write_object");
        assert_eq!(status.state, pb::OperationState::Succeeded as i32);
        assert!(status.created_at.is_some());
        assert!(status.updated_at.is_some());
        assert!(status.progress.is_empty());
        assert!(status.error_summary.is_empty());

        let listed = pb::daemon_server::Daemon::list_operations(
            &service,
            Request::new(pb::ListOperationsRequest {
                filter: Default::default(),
                page_token: None,
                page_size: 0,
            }),
        )
        .await
        .expect("list operations")
        .into_inner();
        assert_eq!(listed.operations, vec![status.clone()]);
        assert!(listed.next_page_token.is_none());

        let filtered = pb::daemon_server::Daemon::list_operations(
            &service,
            Request::new(pb::ListOperationsRequest {
                filter: [("state".to_string(), "succeeded".to_string())]
                    .into_iter()
                    .collect(),
                page_token: None,
                page_size: 0,
            }),
        )
        .await
        .expect("list filtered operations")
        .into_inner();
        assert_eq!(filtered.operations, vec![status]);
    }

    #[tokio::test]
    async fn daemon_reports_queued_operation() {
        let service = state_with_queued_operation().daemon_service();
        let operation_id = operation_uuid();

        let status = pb::daemon_server::Daemon::get_operation(
            &service,
            Request::new(pb::GetOperationRequest {
                operation_id: operation_id.as_bytes().to_vec(),
            }),
        )
        .await
        .expect("get queued operation")
        .into_inner();

        assert_eq!(status.operation_id, operation_id.as_bytes().to_vec());
        assert_eq!(status.operation_kind, "write_object");
        assert_eq!(status.state, pb::OperationState::Queued as i32);
        assert!(status.created_at.is_some());
        assert_eq!(status.created_at, status.updated_at);
        assert!(status.progress.is_empty());
        assert!(status.error_summary.is_empty());
    }

    #[tokio::test]
    async fn watch_streams_until_terminal_and_cancel_flips_token() {
        let state = ApiState::new(test_index());
        let operation_id = Uuid::from_u128(7);
        let handle = state.operations.register(operation_id, "reconcile_tape");
        let daemon = state.daemon_service();

        pb::daemon_server::Daemon::cancel_operation(
            &daemon,
            Request::new(pb::CancelOperationRequest {
                operation_id: operation_id.as_bytes().to_vec(),
                idempotency_key: None,
                force: false,
            }),
        )
        .await
        .expect("cancel operation");
        assert!(handle.is_cancelled());

        handle.publish(crate::operations::status(
            operation_id,
            "reconcile_tape",
            pb::OperationState::Cancelled,
            &[],
        ));
        let mut stream = pb::daemon_server::Daemon::watch_operation(
            &daemon,
            Request::new(pb::GetOperationRequest {
                operation_id: operation_id.as_bytes().to_vec(),
            }),
        )
        .await
        .expect("watch operation")
        .into_inner();
        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.state, pb::OperationState::Cancelled as i32);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn cancel_terminal_live_operation_does_not_regress_durable_state() {
        let state = state_with_operation();
        let operation_id = operation_uuid();
        let handle = state.operations.register(operation_id, "write_object");
        handle.publish(crate::operations::status(
            operation_id,
            "write_object",
            pb::OperationState::Succeeded,
            &[],
        ));
        let daemon = state.daemon_service();

        let cancel = pb::daemon_server::Daemon::cancel_operation(
            &daemon,
            Request::new(pb::CancelOperationRequest {
                operation_id: operation_id.as_bytes().to_vec(),
                idempotency_key: None,
                force: false,
            }),
        )
        .await
        .expect("cancel terminal operation")
        .into_inner();
        assert_eq!(cancel.resulting_state, pb::OperationState::Succeeded as i32);

        let durable = pb::daemon_server::Daemon::get_operation(
            &daemon,
            Request::new(pb::GetOperationRequest {
                operation_id: operation_id.as_bytes().to_vec(),
            }),
        )
        .await
        .expect("get durable operation")
        .into_inner();
        assert_eq!(durable.state, pb::OperationState::Succeeded as i32);
    }

    #[test]
    fn drive_pool_reserves_bays_independently() {
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([
            (0x0101, AtomicBool::new(false)),
            (0x0102, AtomicBool::new(false)),
        ]));
        let pool =
            crate::write_owner::DrivePool::new(changer_tx, HashMap::new(), reservations.clone());

        assert_eq!(pool.reserve_free_drive().expect("first bay"), 0x0101);
        assert_eq!(pool.reserve_free_drive().expect("second bay"), 0x0102);
        assert_eq!(
            pool.reserve_free_drive().expect_err("pool full").code(),
            tonic::Code::FailedPrecondition
        );
        pool.release(0x0101);
        assert_eq!(pool.reserve_free_drive().expect("released bay"), 0x0101);
        assert!(reservations
            .get(&0x0101)
            .expect("reservation")
            .load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn drive_pool_exclusive_reservation_rolls_back_on_busy_bay() {
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([
            (0x0101, AtomicBool::new(false)),
            (0x0102, AtomicBool::new(true)),
            (0x0103, AtomicBool::new(false)),
        ]));
        let pool =
            crate::write_owner::DrivePool::new(changer_tx, HashMap::new(), reservations.clone());

        assert_eq!(
            pool.reserve_all_exclusive()
                .expect_err("busy bay blocks exclusive reservation")
                .code(),
            tonic::Code::FailedPrecondition
        );
        assert!(!reservations
            .get(&0x0101)
            .expect("rolled back")
            .load(std::sync::atomic::Ordering::SeqCst));
        assert!(reservations
            .get(&0x0102)
            .expect("busy remains busy")
            .load(std::sync::atomic::Ordering::SeqCst));
        assert!(!reservations
            .get(&0x0103)
            .expect("unvisited remains free")
            .load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn drive_pool_exclusive_guard_drop_releases_all_bays() {
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([
            (0x0101, AtomicBool::new(false)),
            (0x0102, AtomicBool::new(false)),
        ]));
        let pool =
            crate::write_owner::DrivePool::new(changer_tx, HashMap::new(), reservations.clone());

        pool.reserve_all_exclusive().expect("reserve all");
        assert_eq!(
            pool.reserve_free_drive()
                .expect_err("exclusive reservation holds all bays")
                .code(),
            tonic::Code::FailedPrecondition
        );
        drop(crate::write_owner::ExclusiveGuard::from_reserved(
            reservations.clone(),
        ));
        assert_eq!(pool.reserve_free_drive().expect("released bay"), 0x0101);
    }

    #[test]
    fn drive_pool_tracks_mounted_tapes_for_selection_exclusion() {
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(false))]));
        let pool = crate::write_owner::DrivePool::new(changer_tx, HashMap::new(), reservations);
        let session_id = Uuid::new_v4();

        pool.record_session(
            session_id,
            crate::write_owner::MountedSession {
                bay: 0x0101,
                home_slot: Some(0x1001),
                tape_uuid: TAPE_UUID,
                drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
            },
        );

        assert!(pool.is_tape_mounted(&TAPE_UUID));
        assert!(pool.mounted_tape_uuids().contains(&TAPE_UUID));
        pool.forget_session(session_id);
        assert!(!pool.is_tape_mounted(&TAPE_UUID));
    }

    #[tokio::test]
    async fn drive_byte_accounting_uses_session_drive_uuid_for_shared_bay() {
        let state = ApiState::new(test_index());
        let shared_bay = 0x0101;
        let session_a = crate::write_owner::MountedSession {
            bay: shared_bay,
            home_slot: None,
            tape_uuid: TAPE_UUID,
            drive_uuid: Some(Uuid::from_u128(0x1111).as_bytes().to_vec()),
        };
        let session_b = crate::write_owner::MountedSession {
            bay: shared_bay,
            home_slot: None,
            tape_uuid: TAPE_UUID,
            drive_uuid: Some(Uuid::from_u128(0x2222).as_bytes().to_vec()),
        };
        let mut read_a = CountingBytesStream {
            inner: Box::pin(tokio_stream::iter(vec![Ok(pb::BytesChunk {
                data: b"abc".to_vec(),
                is_last: true,
            })])),
            state: state.clone(),
            drive_uuid: session_a.drive_uuid.clone(),
        };
        let mut read_b = CountingBytesStream {
            inner: Box::pin(tokio_stream::iter(vec![Ok(pb::BytesChunk {
                data: b"defgh".to_vec(),
                is_last: true,
            })])),
            state: state.clone(),
            drive_uuid: session_b.drive_uuid.clone(),
        };

        assert_eq!(session_a.bay, session_b.bay);
        assert!(read_a.next().await.is_some());
        assert!(read_b.next().await.is_some());
        state.record_drive_write_bytes(session_b.drive_uuid.as_deref(), 7);

        let counters = state
            .live_status
            .drive_counters
            .read()
            .unwrap_or_else(|err| err.into_inner());
        assert_eq!(counters.len(), 2);
        let a = counters
            .get(
                session_a
                    .drive_uuid
                    .as_deref()
                    .expect("session A drive uuid"),
            )
            .expect("session A counter");
        let b = counters
            .get(
                session_b
                    .drive_uuid
                    .as_deref()
                    .expect("session B drive uuid"),
            )
            .expect("session B counter");
        assert_eq!(a.read_bytes.load(AtomicOrdering::Relaxed), 3);
        assert_eq!(a.write_bytes.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(b.read_bytes.load(AtomicOrdering::Relaxed), 5);
        assert_eq!(b.write_bytes.load(AtomicOrdering::Relaxed), 7);
    }

    #[test]
    fn drive_byte_accounting_skips_unresolvable_drive_and_warns() {
        let state = ApiState::new(test_index());
        let warnings = capture_warnings(|| {
            state.record_drive_read_bytes(None, 512);
            state.record_drive_write_bytes(Some(&[]), 1024);
        });

        let counters = state
            .live_status
            .drive_counters
            .read()
            .unwrap_or_else(|err| err.into_inner());
        assert!(counters.is_empty());
        assert_eq!(warnings.len(), 2);
        assert!(warnings
            .iter()
            .all(|message| message.contains("skipping byte accounting for unresolved drive")));
    }

    #[test]
    fn drive_pool_tracks_pending_tape_reservations_for_selection_exclusion() {
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(false))]));
        let pool = crate::write_owner::DrivePool::new(changer_tx, HashMap::new(), reservations);

        let reservation = pool.reserve_tape(TAPE_UUID).expect("reserve tape");

        assert!(pool.is_tape_mounted(&TAPE_UUID));
        assert!(pool.mounted_tape_uuids().contains(&TAPE_UUID));
        assert_eq!(
            pool.reserve_tape(TAPE_UUID)
                .expect_err("duplicate tape reservation")
                .code(),
            tonic::Code::FailedPrecondition
        );
        drop(reservation);
        assert!(!pool.is_tape_mounted(&TAPE_UUID));
    }

    #[tokio::test]
    async fn catalog_lists_tapes_and_tape_files() {
        let service = populated_state().catalog_service();

        let tapes = pb::catalog_server::Catalog::list_tapes(
            &service,
            Request::new(pb::ListTapesRequest {
                library_uuid: Vec::new(),
                page_token: None,
                page_size: 0,
                pool_id: String::new(),
                kind: "data".to_string(),
            }),
        )
        .await
        .expect("list tapes")
        .into_inner();
        assert_eq!(tapes.tapes.len(), 1);
        assert_eq!(tapes.tapes[0].tape_uuid, TAPE_UUID.to_vec());
        assert_eq!(tapes.tapes[0].body_format, "rao-v1");
        assert_eq!(tapes.tapes[0].block_size_bytes, 4096);
        assert_eq!(tapes.tapes[0].last_committed_tape_file, 7);
        assert_eq!(tapes.tapes[0].state, pb::tape::State::TapeStateReady as i32);
        assert_eq!(tapes.tapes[0].pool_id, "camera.copy-a");

        let filtered_tapes = pb::catalog_server::Catalog::list_tapes(
            &service,
            Request::new(pb::ListTapesRequest {
                library_uuid: Vec::new(),
                page_token: None,
                page_size: 0,
                pool_id: "camera.copy-a".to_string(),
                kind: "data".to_string(),
            }),
        )
        .await
        .expect("list tapes by pool")
        .into_inner();
        assert_eq!(filtered_tapes.tapes, tapes.tapes);

        let pools = pb::catalog_server::Catalog::list_tape_pools(
            &service,
            Request::new(pb::ListTapePoolsRequest {
                page_token: None,
                page_size: 0,
            }),
        )
        .await
        .expect("list tape pools")
        .into_inner();
        assert_eq!(pools.pools.len(), 1);
        assert_eq!(pools.pools[0].pool_id, "camera.copy-a");
        assert_eq!(pools.pools[0].display_name, "Camera copy A");
        assert_eq!(pools.pools[0].copy_class, "copy-a");
        assert_eq!(pools.pools[0].content_class, "camera");

        let pool = pb::catalog_server::Catalog::get_tape_pool(
            &service,
            Request::new(pb::GetTapePoolRequest {
                pool_id: "camera.copy-a".to_string(),
            }),
        )
        .await
        .expect("get tape pool")
        .into_inner();
        assert_eq!(pool, pools.pools[0]);

        let invalid_pool = pb::catalog_server::Catalog::list_tapes(
            &service,
            Request::new(pb::ListTapesRequest {
                library_uuid: Vec::new(),
                page_token: None,
                page_size: 0,
                pool_id: "camera copy a".to_string(),
                kind: "data".to_string(),
            }),
        )
        .await
        .expect_err("invalid pool id must fail");
        assert_eq!(invalid_pool.code(), tonic::Code::InvalidArgument);

        let tape = pb::catalog_server::Catalog::get_tape(
            &service,
            Request::new(pb::GetTapeRequest {
                tape_uuid: TAPE_UUID.to_vec(),
            }),
        )
        .await
        .expect("get tape")
        .into_inner();
        assert_eq!(tape.tape_uuid, TAPE_UUID.to_vec());

        let files = pb::catalog_server::Catalog::list_tape_files(
            &service,
            Request::new(pb::ListTapeFilesRequest {
                tape_uuid: TAPE_UUID.to_vec(),
                page_token: None,
                page_size: 0,
            }),
        )
        .await
        .expect("list tape files")
        .into_inner();
        assert_eq!(files.tape_files.len(), 4);
        assert_eq!(files.tape_files[0].kind, "object");
        assert_eq!(
            files.tape_files[0].object_id,
            object_uuid().as_bytes().to_vec()
        );
        assert_eq!(files.tape_files[1].kind, "parity_sidecar");
        assert_eq!(files.tape_files[2].kind, "parity_map");
        assert_eq!(files.tape_files[3].kind, "bootstrap");
    }

    #[tokio::test]
    async fn catalog_lists_and_fetches_files_in_native_object() {
        let service = populated_state_with_file_catalog().catalog_service();

        let files = pb::catalog_server::Catalog::list_files_in_object(
            &service,
            Request::new(pb::ListFilesInObjectRequest {
                object_id: object_uuid().as_bytes().to_vec(),
                page_token: None,
                page_size: 0,
            }),
        )
        .await
        .expect("list object files")
        .into_inner();
        assert_eq!(files.files.len(), 1);
        let file = &files.files[0];
        assert_eq!(file.object_id, object_uuid().as_bytes().to_vec());
        assert_eq!(file.file_id, b"file-camera");
        assert_eq!(file.path, "payload.bin");
        assert_eq!(file.size_bytes, 17);
        assert_eq!(file.file_sha256, vec![7u8; 32]);
        assert_eq!(file.first_chunk_body_lba, 2);
        assert_eq!(file.chunk_count, 1);

        let by_path = pb::catalog_server::Catalog::get_file(
            &service,
            Request::new(pb::GetFileRequest {
                object_id: object_uuid().as_bytes().to_vec(),
                key: Some(pb::get_file_request::Key::Path("payload.bin".to_string())),
            }),
        )
        .await
        .expect("get file by path")
        .into_inner();
        assert_eq!(by_path, *file);

        let by_id = pb::catalog_server::Catalog::get_file(
            &service,
            Request::new(pb::GetFileRequest {
                object_id: object_uuid().as_bytes().to_vec(),
                key: Some(pb::get_file_request::Key::FileId(b"file-camera".to_vec())),
            }),
        )
        .await
        .expect("get file by id")
        .into_inner();
        assert_eq!(by_id, *file);
    }

    #[tokio::test]
    async fn catalog_enumerates_and_fetches_native_objects() {
        let service = populated_state().catalog_service();
        let mut stream = pb::catalog_server::Catalog::enumerate_objects(
            &service,
            Request::new(pb::EnumerateObjectsRequest {
                scope: Some(pb::enumerate_objects_request::Scope::All(())),
                reconcile_from_tape: false,
            }),
        )
        .await
        .expect("enumerate objects")
        .into_inner();

        let first = stream
            .next()
            .await
            .expect("one object")
            .expect("object record");
        assert_eq!(first.object_id, object_uuid().as_bytes().to_vec());
        assert_eq!(first.caller_object_id, "caller-1");
        assert_eq!(first.body_format, "rao-v1");
        assert_eq!(first.logical_size_bytes, 17);
        assert_eq!(first.content_sha256, vec![7u8; 32]);
        assert_eq!(first.copies.len(), 1);
        assert_eq!(first.copies[0].pool_id, "camera.copy-a");
        assert!(stream.next().await.is_none());

        let fetched = pb::catalog_server::Catalog::get_object(
            &service,
            Request::new(pb::GetObjectRequest {
                key: Some(pb::get_object_request::Key::CallerObjectId(
                    "caller-1".to_string(),
                )),
            }),
        )
        .await
        .expect("get object")
        .into_inner();
        assert_eq!(fetched.object_id, object_uuid().as_bytes().to_vec());

        let fetched_by_id = pb::catalog_server::Catalog::get_object(
            &service,
            Request::new(pb::GetObjectRequest {
                key: Some(pb::get_object_request::Key::ObjectId(
                    object_uuid().as_bytes().to_vec(),
                )),
            }),
        )
        .await
        .expect("get object by uuid")
        .into_inner();
        assert_eq!(fetched_by_id.object_id, object_uuid().as_bytes().to_vec());

        let copies = pb::catalog_server::Catalog::find_object_copies(
            &service,
            Request::new(pb::FindObjectCopiesRequest {
                key: Some(pb::find_object_copies_request::Key::ContentSha256(vec![
                    7u8;
                    32
                ])),
            }),
        )
        .await
        .expect("find copies")
        .into_inner();
        assert_eq!(copies.copies.len(), 1);
        assert_eq!(copies.copies[0].tape_uuid, vec![3u8; 16]);
        assert_eq!(copies.copies[0].tape_file_number, 4);
        assert_eq!(copies.copies[0].pool_id, "camera.copy-a");
    }

    #[tokio::test]
    async fn write_session_tape_target_is_unimplemented_in_s4a() {
        let service = empty_pool_state().write_session_service();
        let err = pb::write_session_service_server::WriteSessionService::open_write_session(
            &service,
            Request::new(pb::OpenWriteSessionRequest {
                target: Some(pb::open_write_session_request::Target::TapeTarget(
                    pb::TapeTarget {
                        tape_uuid: TAPE_UUID.to_vec(),
                        mount_if_needed: false,
                        required_pool_id: "camera.copy-b".to_string(),
                    },
                )),
                body_format: "rao-v1".to_string(),
                idempotency_key: None,
                recover_session_id: Vec::new(),
            }),
        )
        .await
        .expect_err("tape target is out of scope for S4a");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    #[test]
    fn pool_target_library_uuid_resolves_snapshot_uuid() {
        let service = state_with_library_snapshot("LIB001").write_session_service();
        let target = pb::TapePoolTarget {
            pool_id: "camera.copy-a".to_string(),
            library_uuid: crate::library::library_uuid("LIB001").to_vec(),
            mount_if_needed: true,
        };

        let serial = service
            .library_serial_for_pool_target(&target)
            .expect("library UUID resolves to serial");

        assert_eq!(serial, "LIB001");
    }

    #[test]
    fn pool_target_library_uuid_rejects_legacy_raw_serial_bytes() {
        let service = state_with_library_snapshot("LIB001").write_session_service();
        let target = pb::TapePoolTarget {
            pool_id: "camera.copy-a".to_string(),
            library_uuid: b"LIB001".to_vec(),
            mount_if_needed: true,
        };

        let err = service
            .library_serial_for_pool_target(&target)
            .expect_err("library_uuid is a 16-byte UUID, not a serial string");

        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn write_session_pool_target_rejects_legacy_raw_library_serial() {
        let service = empty_pool_state().write_session_service();
        let err = pb::write_session_service_server::WriteSessionService::open_write_session(
            &service,
            Request::new(pb::OpenWriteSessionRequest {
                target: Some(pb::open_write_session_request::Target::PoolTarget(
                    pb::TapePoolTarget {
                        pool_id: "camera.copy-a".to_string(),
                        library_uuid: b"LIB001".to_vec(),
                        mount_if_needed: true,
                    },
                )),
                body_format: "rao-v1".to_string(),
                idempotency_key: None,
                recover_session_id: Vec::new(),
            }),
        )
        .await
        .expect_err("legacy raw serial is not a library_uuid");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn read_session_read_only_state_returns_unavailable_for_tape_open() {
        let service = populated_state().read_session_service();
        let err = pb::read_session_service_server::ReadSessionService::open_read_session(
            &service,
            Request::new(pb::OpenReadSessionRequest {
                target: Some(pb::open_read_session_request::Target::TapeTarget(
                    pb::TapeTarget {
                        tape_uuid: TAPE_UUID.to_vec(),
                        mount_if_needed: true,
                        required_pool_id: "camera.copy-a".to_string(),
                    },
                )),
                idempotency_key: None,
            }),
        )
        .await
        .expect_err("read-only ApiState has no session owner");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn read_object_range_dispatches_empty_file_id_range_to_drive_actor() {
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(true))]));
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            HashMap::from([(0x0101, drive_tx)]),
            reservations,
        );
        let session_id = Uuid::new_v4();
        pool.record_session(
            session_id,
            crate::write_owner::MountedSession {
                bay: 0x0101,
                home_slot: None,
                tape_uuid: TAPE_UUID,
                drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
            },
        );
        let mut state = populated_state();
        state.drive_pool = Some(pool);
        let service = state.read_session_service();

        let drive_task = tokio::spawn(async move {
            let command = drive_rx.recv().await.expect("drive command");
            let crate::write_owner::DriveCommand::ReadObjectRange {
                session_id: got_session_id,
                object_id,
                file_id,
                start_byte,
                end_byte,
                stream_chunk_bytes,
                chunk_tx,
            } = command
            else {
                panic!("expected ReadObjectRange command");
            };
            assert_eq!(got_session_id, session_id);
            assert_eq!(object_id, OBJECT_ID_TEXT);
            assert_eq!(file_id, "");
            assert_eq!(start_byte, 1);
            assert_eq!(end_byte, 5);
            assert_eq!(stream_chunk_bytes, 4);
            chunk_tx
                .send(Ok(pb::BytesChunk {
                    data: b"ANGE".to_vec(),
                    is_last: false,
                }))
                .await
                .expect("send data chunk");
            chunk_tx
                .send(Ok(pb::BytesChunk {
                    data: Vec::new(),
                    is_last: true,
                }))
                .await
                .expect("send last chunk");
        });

        let mut stream = pb::read_session_service_server::ReadSessionService::read_object_range(
            &service,
            Request::new(pb::ReadObjectRangeRequest {
                session_id: session_id.as_bytes().to_vec(),
                object_id: object_uuid().as_bytes().to_vec(),
                file_id: Vec::new(),
                start_byte: 1,
                end_byte: 5,
                stream_chunk_bytes: 4,
            }),
        )
        .await
        .expect("range stream")
        .into_inner();

        let mut got = Vec::new();
        let mut saw_last = false;
        while let Some(item) = stream.next().await {
            let chunk = item.expect("chunk");
            got.extend_from_slice(&chunk.data);
            saw_last |= chunk.is_last;
        }
        drive_task.await.expect("drive task");
        assert_eq!(got, b"ANGE");
        assert!(saw_last);
    }

    #[tokio::test]
    async fn read_object_range_empty_file_id_zero_zero_uses_whole_payload_path() {
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(true))]));
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            HashMap::from([(0x0101, drive_tx)]),
            reservations,
        );
        let session_id = Uuid::new_v4();
        pool.record_session(
            session_id,
            crate::write_owner::MountedSession {
                bay: 0x0101,
                home_slot: None,
                tape_uuid: TAPE_UUID,
                drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
            },
        );
        let mut state = populated_state();
        state.drive_pool = Some(pool);
        let service = state.read_session_service();

        let drive_task = tokio::spawn(async move {
            let command = drive_rx.recv().await.expect("drive command");
            let crate::write_owner::DriveCommand::ReadFile {
                session_id: got_session_id,
                object_id,
                file_id,
                stream_chunk_bytes,
                chunk_tx,
            } = command
            else {
                panic!("expected ReadFile command");
            };
            assert_eq!(got_session_id, session_id);
            assert_eq!(object_id, OBJECT_ID_TEXT);
            assert!(file_id.is_empty());
            assert_eq!(stream_chunk_bytes, 6);
            chunk_tx
                .send(Ok(pb::BytesChunk {
                    data: b"whole payload".to_vec(),
                    is_last: false,
                }))
                .await
                .expect("send data chunk");
            chunk_tx
                .send(Ok(pb::BytesChunk {
                    data: Vec::new(),
                    is_last: true,
                }))
                .await
                .expect("send last chunk");
        });

        let mut stream = pb::read_session_service_server::ReadSessionService::read_object_range(
            &service,
            Request::new(pb::ReadObjectRangeRequest {
                session_id: session_id.as_bytes().to_vec(),
                object_id: object_uuid().as_bytes().to_vec(),
                file_id: Vec::new(),
                start_byte: 0,
                end_byte: 0,
                stream_chunk_bytes: 6,
            }),
        )
        .await
        .expect("whole payload stream")
        .into_inner();

        let mut got = Vec::new();
        let mut saw_last = false;
        while let Some(item) = stream.next().await {
            let chunk = item.expect("chunk");
            got.extend_from_slice(&chunk.data);
            saw_last |= chunk.is_last;
        }
        drive_task.await.expect("drive task");
        assert_eq!(got, b"whole payload");
        assert!(saw_last);
    }

    #[tokio::test]
    async fn read_object_range_dispatches_file_scoped_range_to_drive_actor() {
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(true))]));
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            HashMap::from([(0x0101, drive_tx)]),
            reservations,
        );
        let session_id = Uuid::new_v4();
        pool.record_session(
            session_id,
            crate::write_owner::MountedSession {
                bay: 0x0101,
                home_slot: None,
                tape_uuid: TAPE_UUID,
                drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
            },
        );
        let mut state = populated_state();
        state.drive_pool = Some(pool);
        let service = state.read_session_service();

        let drive_task = tokio::spawn(async move {
            let command = drive_rx.recv().await.expect("drive command");
            let crate::write_owner::DriveCommand::ReadObjectRange {
                session_id: got_session_id,
                object_id,
                file_id,
                start_byte,
                end_byte,
                stream_chunk_bytes,
                chunk_tx,
            } = command
            else {
                panic!("expected ReadObjectRange command");
            };
            assert_eq!(got_session_id, session_id);
            assert_eq!(object_id, OBJECT_ID_TEXT);
            assert_eq!(file_id, "file-camera");
            assert_eq!(start_byte, 5);
            assert_eq!(end_byte, 9);
            assert_eq!(stream_chunk_bytes, 3);
            chunk_tx
                .send(Ok(pb::BytesChunk {
                    data: b"nge".to_vec(),
                    is_last: false,
                }))
                .await
                .expect("send data chunk");
            chunk_tx
                .send(Ok(pb::BytesChunk {
                    data: Vec::new(),
                    is_last: true,
                }))
                .await
                .expect("send last chunk");
        });

        let mut stream = pb::read_session_service_server::ReadSessionService::read_object_range(
            &service,
            Request::new(pb::ReadObjectRangeRequest {
                session_id: session_id.as_bytes().to_vec(),
                object_id: object_uuid().as_bytes().to_vec(),
                file_id: b"file-camera".to_vec(),
                start_byte: 5,
                end_byte: 9,
                stream_chunk_bytes: 3,
            }),
        )
        .await
        .expect("range stream")
        .into_inner();

        let mut got = Vec::new();
        let mut saw_last = false;
        while let Some(item) = stream.next().await {
            let chunk = item.expect("chunk");
            got.extend_from_slice(&chunk.data);
            saw_last |= chunk.is_last;
        }
        drive_task.await.expect("drive task");
        assert_eq!(got, b"nge");
        assert!(saw_last);
    }

    #[tokio::test]
    async fn read_file_dispatches_file_id_as_whole_file_range() {
        let (changer_tx, _changer_rx) = tokio::sync::mpsc::channel(1);
        let (drive_tx, mut drive_rx) = tokio::sync::mpsc::channel(1);
        let reservations = Arc::new(HashMap::from([(0x0101, AtomicBool::new(true))]));
        let pool = crate::write_owner::DrivePool::new(
            changer_tx,
            HashMap::from([(0x0101, drive_tx)]),
            reservations,
        );
        let session_id = Uuid::new_v4();
        pool.record_session(
            session_id,
            crate::write_owner::MountedSession {
                bay: 0x0101,
                home_slot: None,
                tape_uuid: TAPE_UUID,
                drive_uuid: Some(Uuid::new_v4().as_bytes().to_vec()),
            },
        );
        let mut state = populated_state();
        state.drive_pool = Some(pool);
        let service = state.read_session_service();

        let drive_task = tokio::spawn(async move {
            let command = drive_rx.recv().await.expect("drive command");
            let crate::write_owner::DriveCommand::ReadObjectRange {
                file_id,
                start_byte,
                end_byte,
                chunk_tx,
                ..
            } = command
            else {
                panic!("expected ReadObjectRange command");
            };
            assert_eq!(file_id, "file-camera");
            assert_eq!(start_byte, 0);
            assert_eq!(end_byte, 0);
            chunk_tx
                .send(Ok(pb::BytesChunk {
                    data: Vec::new(),
                    is_last: true,
                }))
                .await
                .expect("send last chunk");
        });

        let mut stream = pb::read_session_service_server::ReadSessionService::read_file(
            &service,
            Request::new(pb::ReadFileRequest {
                session_id: session_id.as_bytes().to_vec(),
                object_id: object_uuid().as_bytes().to_vec(),
                file_id: b"file-camera".to_vec(),
                stream_chunk_bytes: 0,
            }),
        )
        .await
        .expect("read file stream")
        .into_inner();

        assert!(stream.next().await.expect("last").expect("chunk").is_last);
        assert!(stream.next().await.is_none());
        drive_task.await.expect("drive task");
    }

    #[tokio::test]
    async fn catalog_units_are_exposed_as_parallel_surface() {
        let service = populated_state().catalog_service();
        let mut stream = pb::catalog_server::Catalog::enumerate_units(
            &service,
            Request::new(pb::EnumerateUnitsRequest {
                scope: Some(pb::enumerate_units_request::Scope::All(())),
                origin_filter: pb::CatalogUnitOriginFilter::NativeObjects as i32,
                refresh_from_source: false,
            }),
        )
        .await
        .expect("enumerate units")
        .into_inner();

        let unit = stream
            .next()
            .await
            .expect("one unit")
            .expect("catalog unit");
        assert_eq!(
            unit.origin_kind,
            pb::CatalogUnitOriginKind::NativeObject as i32
        );
        assert_eq!(unit.format_id, "rao-v1");
        assert!(matches!(
            unit.origin,
            Some(pb::catalog_unit::Origin::Native(
                pb::NativeUnitSummary { .. }
            ))
        ));

        let fetched = pb::catalog_server::Catalog::get_catalog_unit(
            &service,
            Request::new(pb::GetCatalogUnitRequest {
                unit_id: unit.unit_id.clone(),
            }),
        )
        .await
        .expect("get catalog unit")
        .into_inner();
        assert_eq!(fetched.unit_id, unit.unit_id);

        let err = pb::catalog_server::Catalog::list_entries_in_unit(
            &service,
            Request::new(pb::ListEntriesInUnitRequest {
                unit_id: fetched.unit_id,
                page_token: None,
                page_size: 0,
                refresh_from_source: false,
            }),
        )
        .await
        .expect_err("entry listing is deliberately not wired yet");
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    #[cfg(feature = "foreign-bru")]
    #[tokio::test]
    async fn foreign_bru_dump_unit_lists_normalized_entries() {
        let (state, unit_id, source_id) = foreign_bru_state();
        let service = state.catalog_service();
        let mut stream = pb::catalog_server::Catalog::enumerate_units(
            &service,
            Request::new(pb::EnumerateUnitsRequest {
                scope: Some(pb::enumerate_units_request::Scope::All(())),
                origin_filter: pb::CatalogUnitOriginFilter::ForeignArchives as i32,
                refresh_from_source: false,
            }),
        )
        .await
        .expect("enumerate foreign units")
        .into_inner();

        let unit = stream
            .next()
            .await
            .expect("one foreign unit")
            .expect("foreign unit");
        assert_eq!(unit.unit_id, unit_id.as_bytes().to_vec());
        assert_eq!(
            unit.origin_kind,
            pb::CatalogUnitOriginKind::ForeignArchive as i32
        );
        assert_eq!(unit.format_id, "remanence-bru");
        let Some(pb::catalog_unit::Origin::Foreign(summary)) = unit.origin else {
            panic!("foreign summary expected");
        };
        assert_eq!(summary.scan_id, b"scan-bru-1".to_vec());
        assert_eq!(summary.source_kind, "byte_stream_dump");
        assert_eq!(summary.source_id, source_id);
        assert_eq!(summary.entry_count, 1);
        assert_eq!(summary.damage_event_count, 0);
        assert!(stream.next().await.is_none());

        let entries = pb::catalog_server::Catalog::list_entries_in_unit(
            &service,
            Request::new(pb::ListEntriesInUnitRequest {
                unit_id: unit_id.as_bytes().to_vec(),
                page_token: None,
                page_size: 0,
                refresh_from_source: false,
            }),
        )
        .await
        .expect("list foreign unit entries")
        .into_inner();
        assert_eq!(entries.entries.len(), 1);
        assert_eq!(entries.entries[0].path, "camera/a.txt");
        assert_eq!(
            entries.entries[0].kind,
            pb::CatalogEntryKind::RegularFile as i32
        );
        assert_eq!(entries.entries[0].size_bytes, Some(3));
        assert_eq!(
            entries.entries[0].state,
            pb::CatalogEntryState::Complete as i32
        );
        assert_eq!(
            entries.entries[0].integrity_basis,
            pb::IntegrityBasis::FormatChecksum as i32
        );
        assert!(entries.archive_gaps.is_empty());
    }

    #[cfg(feature = "foreign-bru")]
    #[tokio::test]
    async fn foreign_bru_dump_unit_surfaces_archive_gaps() {
        let (state, unit_id, _source_id) = foreign_bru_state_with_gap(true);
        let service = state.catalog_service();

        let entries = pb::catalog_server::Catalog::list_entries_in_unit(
            &service,
            Request::new(pb::ListEntriesInUnitRequest {
                unit_id: unit_id.as_bytes().to_vec(),
                page_token: None,
                page_size: 0,
                refresh_from_source: false,
            }),
        )
        .await
        .expect("list foreign unit entries")
        .into_inner();

        assert_eq!(entries.entries.len(), 1);
        assert_eq!(entries.entries[0].path, "camera/a.txt");
        assert_eq!(entries.archive_gaps.len(), 1);
        assert_eq!(entries.archive_gaps[0].unit_id, unit_id.as_bytes().to_vec());
        assert_eq!(entries.archive_gaps[0].source_start, BRU_BLOCK_SIZE as u64);
        assert_eq!(
            entries.archive_gaps[0].source_end,
            (BRU_BLOCK_SIZE * 2) as u64
        );
        assert_eq!(
            entries.archive_gaps[0].cause,
            pb::ArchiveGapCause::UnrecognizedData as i32
        );
    }

    #[cfg(not(feature = "foreign-bru"))]
    #[tokio::test]
    async fn foreign_bru_dump_unit_reports_unavailable_without_plugin() {
        let mut index = test_index();
        let unit_id = index
            .upsert_foreign_archive_projection(ForeignArchiveProjectionInput {
                tape_uuid: Vec::new(),
                format_id: "remanence-bru".to_string(),
                scan_id: "scan-bru-1".to_string(),
                source_kind: "byte_stream_dump".to_string(),
                source_id: "/nonexistent/fixture.bru".to_string(),
                confidence: "high".to_string(),
                entry_count: 0,
                damage_event_count: 0,
                adapter_state: vec![],
                last_scan_at_utc: Some("2026-05-28T13:15:00Z".to_string()),
                created_at_utc: Some("2026-05-28T13:15:01Z".to_string()),
            })
            .expect("project foreign BRU unit");
        let service = ApiState::new(index).catalog_service();

        let error = pb::catalog_server::Catalog::list_entries_in_unit(
            &service,
            Request::new(pb::ListEntriesInUnitRequest {
                unit_id: unit_id.as_bytes().to_vec(),
                page_token: None,
                page_size: 0,
                refresh_from_source: false,
            }),
        )
        .await
        .expect_err("BRU plugin is unavailable by default");

        assert_eq!(error.code(), tonic::Code::Unimplemented);
        assert!(error
            .message()
            .contains("format remanence-bru is not available in this build"));
    }
}
