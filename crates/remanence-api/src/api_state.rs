//! Shared API state construction, service factories, and core catalog accessors.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use ciborium::value::Value as CborValue;
use remanence_format_driver::ForeignFormatRegistry;
use remanence_state::{
    AuditActor, AuditEvent, CalibrationControlStore, CatalogIndex, RemConfig, TapePoolConfig,
};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tonic::Status;
use uuid::Uuid;

use crate::audit_projection::{append_operation_audit, OperationAuditInput};
use crate::audit_query_service::AuditApi;
use crate::catalog_conversion::alarm_record_to_proto;
use crate::daemon_catalog_services::{CatalogService, DaemonService};
use crate::drive_collection::{
    drive_managed_library_serials, observe_drive_catalog_from_libraries,
    spawn_drive_collection_workers,
};
use crate::hex_encoding::bytes_to_hex;
use crate::library::LibraryServiceApi;
use crate::live_status::{DriveByteCounters, LibrarySnapshot, LiveStatusState};
use crate::pb;
use crate::read_plan::ReadPlanApi;
use crate::read_session_service::ReadSessionApi;
use crate::startup_checkpoint::{
    default_audit_dir_for_index, default_calibration_dir_for_index,
    default_checkpoint_journal_dir_for_index, live_status_config_from,
    open_calibration_store_for_config, replay_checkpoint_journal_projections_with_audit,
};
use crate::startup_guard::{reject_active_tape_io_fences_on_startup, tape_io_runtime_config};
use crate::startup_media_readiness::reconcile_library_media_readiness_on_startup;
use crate::startup_media_readiness::status_from_state_error;
use crate::write_session_ingress::WriteSessionApi;

/// Shared state for the initial Layer 5 service implementations.
#[derive(Clone)]
pub struct ApiState {
    pub(crate) index_path: Arc<PathBuf>,
    pub(crate) audit_dir: Arc<PathBuf>,
    pub(crate) audit_fsync: bool,
    pub(crate) audit_append_lock: Arc<std::sync::Mutex<()>>,
    pub(crate) operations: crate::operations::OperationRegistry,
    pub(crate) pool_configs: Arc<HashMap<String, TapePoolConfig>>,
    pub(crate) managed_library_serials: Arc<HashSet<String>>,
    pub(crate) drive_pool: Option<crate::write_owner::DrivePool>,
    pub(crate) spool_dir: Option<Arc<PathBuf>>,
    pub(crate) spool_budget_bytes: Option<u64>,
    pub(crate) io_memory: Arc<crate::io_memory::IoMemoryReservation>,
    pub(crate) append_staging_mode: remanence_state::AppendStagingMode,
    pub(crate) append_ring_bytes: u64,
    pub(crate) append_ring_high_pct: u8,
    pub(crate) append_ring_low_pct: u8,
    pub(crate) checkpoint_journal_dir: Arc<PathBuf>,
    pub(crate) checkpoint_max_bytes: u64,
    pub(crate) checkpoint_max_objects: u64,
    pub(crate) checkpoint_max_age_seconds: u64,
    pub(crate) default_library_serial: Option<Arc<String>>,
    pub(crate) library_snapshot: Option<Arc<RwLock<Arc<LibrarySnapshot>>>>,
    pub(crate) live_status: Arc<LiveStatusState>,
    pub(crate) drive_idle_unload_seconds: u64,
    pub(crate) daemon_epoch: u64,
    pub(crate) daemon_version: String,
    pub(crate) api_version: String,
    pub(crate) rust_target: String,
    pub(crate) foreign_formats: ForeignFormatRegistry,
    /// Durable calibration-control store handle (wrap-map read
    /// ordering, design-read-ordering.md §§4.3/6.5). One instance per
    /// process; clones share state.
    pub(crate) calibration_store: CalibrationControlStore,
}

/// How the drive catalog answered for a changer-reported bay.
///
/// The variants exist so a caller cannot silently skip the "no answer" case:
/// the previous code returned `Option<DriveRecord>` and simply did nothing on
/// `None`, which left the projected row carrying an empty identity that looked
/// like a value.
pub(crate) enum BayResolution {
    /// One active catalog row claims the bay.
    Resolved(remanence_state::DriveRecord),
    /// The bay's only claimant is retired. Identity is known; the drive is
    /// excluded from every selection path by `state`.
    Retired(remanence_state::DriveRecord),
    /// The changer reports the bay; the catalog holds no row for it.
    Uncatalogued,
    /// More than one row claims the bay. The chosen row is the preferred one.
    Ambiguous(remanence_state::DriveRecord),
}

impl ApiState {
    /// Build service state around an already-opened rebuildable catalog index.
    pub fn new(index: CatalogIndex) -> Self {
        Self::new_with_pool_configs(index, Vec::new())
    }

    /// Build service state with operator-resolved tape-pool selection config.
    pub fn new_with_config(mut index: CatalogIndex, config: &RemConfig) -> Result<Self, Status> {
        let audit_append_lock = Arc::new(std::sync::Mutex::new(()));
        replay_checkpoint_journal_projections_with_audit(
            &mut index,
            config.journal.dir.join("checkpoints").as_path(),
            config.audit.dir.as_path(),
            &audit_append_lock,
        )?;
        let index_path = index.path().to_path_buf();
        let pool_configs = config
            .tape_pools
            .clone()
            .into_iter()
            .map(|pool| (pool.id.trim().to_string(), pool))
            .collect();
        let calibration_store = open_calibration_store_for_config(config)?;
        let mut state = Self::new_with_pool_configs_inner(
            index_path,
            pool_configs,
            // Configured-or-daemon-operated set (never raw config.drives —
            // its empty default would trip library_is_managed's empty⇒all
            // fallback and mark foreign libraries managed).
            drive_managed_library_serials(config),
            config.audit.dir.clone(),
            config.audit.fsync,
            audit_append_lock,
            live_status_config_from(&config.livestatus),
            calibration_store,
        );
        state.append_staging_mode = config.daemon.append_staging_mode;
        state.append_ring_bytes = config.daemon.append_ring_bytes;
        state.append_ring_high_pct = config.daemon.append_ring_high_pct;
        state.append_ring_low_pct = config.daemon.append_ring_low_pct;
        state.checkpoint_journal_dir = Arc::new(config.journal.dir.join("checkpoints"));
        state.checkpoint_max_bytes = config.daemon.checkpoint_max_bytes;
        state.checkpoint_max_objects = config.daemon.checkpoint_max_objects;
        state.checkpoint_max_age_seconds = config.daemon.checkpoint_max_age_seconds;
        Ok(state)
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
        let calibration_store =
            CalibrationControlStore::open(default_calibration_dir_for_index(index_path.as_path()))
                .expect("open calibration-control store beside the index");
        Self::new_with_pool_configs_inner(
            index_path,
            pool_configs,
            HashSet::new(),
            audit_dir,
            false,
            Arc::new(std::sync::Mutex::new(())),
            live_status_config_from(&remanence_state::LiveStatusConfig::default()),
            calibration_store,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_pool_configs_inner(
        index_path: PathBuf,
        pool_configs: HashMap<String, TapePoolConfig>,
        managed_library_serials: HashSet<String>,
        audit_dir: PathBuf,
        audit_fsync: bool,
        audit_append_lock: Arc<std::sync::Mutex<()>>,
        live_status_interval: Duration,
        calibration_store: CalibrationControlStore,
    ) -> Self {
        let daemon_epoch = Uuid::new_v4().as_u128() as u64;
        let checkpoint_journal_dir = default_checkpoint_journal_dir_for_index(index_path.as_path());
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
            spool_budget_bytes: None,
            io_memory: crate::io_memory::IoMemoryReservation::new(
                remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
            )
            .expect("nonzero default I/O memory ceiling"),
            append_staging_mode: remanence_state::AppendStagingMode::Serial,
            append_ring_bytes: remanence_state::DEFAULT_APPEND_RING_BYTES,
            append_ring_high_pct: 90,
            append_ring_low_pct: 25,
            checkpoint_journal_dir: Arc::new(checkpoint_journal_dir),
            checkpoint_max_bytes: remanence_state::DEFAULT_CHECKPOINT_MAX_BYTES,
            checkpoint_max_objects: remanence_state::DEFAULT_CHECKPOINT_MAX_OBJECTS,
            checkpoint_max_age_seconds: remanence_state::DEFAULT_CHECKPOINT_MAX_AGE_SECONDS,
            default_library_serial: None,
            library_snapshot: None,
            live_status: Arc::new(LiveStatusState::new(live_status_interval)),
            drive_idle_unload_seconds: remanence_state::DEFAULT_DRIVE_IDLE_UNLOAD_SECONDS,
            daemon_epoch,
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            api_version: "v1-draft".to_string(),
            rust_target: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            foreign_formats: ForeignFormatRegistry::default(),
            calibration_store,
        }
    }

    /// Durable calibration-control store handle (cloneable).
    pub(crate) fn calibration_store(&self) -> &CalibrationControlStore {
        &self.calibration_store
    }

    /// Attach the read-only foreign formats linked by this distribution.
    pub fn with_foreign_formats(mut self, registry: ForeignFormatRegistry) -> Self {
        self.foreign_formats = registry;
        self
    }

    /// Build service state with a live changer/drive actor pool.
    pub fn with_drive_pool(
        mut index: CatalogIndex,
        config: &RemConfig,
        report: remanence_library::DiscoveryReport,
        policy: remanence_library::StaticAllowlist,
        spool_dir: PathBuf,
        spool_budget_bytes: u64,
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
        if config.libraries.is_empty() {
            return Err(Status::invalid_argument(
                "drive-pool daemon mode requires at least one configured library",
            ));
        }
        let mut opened_libraries = Vec::new();
        for configured in &config.libraries {
            let library_serial = configured.serial.trim();
            let discovered = report.library(library_serial).ok_or_else(|| {
                Status::not_found(format!(
                    "configured library {library_serial} not found in discovery report"
                ))
            })?;
            let mut library = discovered
                .open(&policy)
                .map_err(|err| Status::internal(format!("open library {library_serial}: {err}")))?;
            let mut opened_drives = Vec::new();
            for bay in library.library().drive_bays.clone() {
                let Some(installed) = bay.installed.as_ref() else {
                    continue;
                };
                if installed.sg_path.is_none() {
                    continue;
                }
                let bay_addr = bay.element_address;
                let drive = library
                    .open_drive_with_tape_io(
                        bay_addr,
                        &policy,
                        tape_io_runtime_config(&config.tape_io),
                    )
                    .map_err(|err| {
                        Status::internal(format!(
                            "open library {library_serial} drive bay 0x{bay_addr:04x}: {err}"
                        ))
                    })?;
                opened_drives.push((bay_addr, drive));
            }
            if opened_drives.is_empty() {
                return Err(Status::failed_precondition(format!(
                    "configured library {library_serial} has no openable drives"
                )));
            }
            reconcile_library_media_readiness_on_startup(
                &mut index,
                library.library(),
                &mut opened_drives,
            )?;
            opened_libraries.push((library_serial.to_string(), library, opened_drives));
        }
        replay_checkpoint_journal_projections_with_audit(
            &mut index,
            config.journal.dir.join("checkpoints").as_path(),
            config.audit.dir.as_path(),
            &audit_append_lock,
        )?;
        reject_active_tape_io_fences_on_startup(&index)?;
        let reservations = Arc::new(
            opened_libraries
                .iter()
                .flat_map(|(library_serial, _, drives)| {
                    drives.iter().map(|(bay, _)| {
                        (
                            crate::drive_pool::DriveKey::new(library_serial.clone(), *bay),
                            AtomicBool::new(false),
                        )
                    })
                })
                .collect::<HashMap<_, _>>(),
        );
        let managed_library_serials = drive_managed_library_serials(config);
        let io_memory = crate::io_memory::IoMemoryReservation::new(config.daemon.io_memory_ceiling)
            .map_err(Status::invalid_argument)?;
        let (timer_park_tx, timer_park_rx) = tokio::sync::mpsc::unbounded_channel();
        let drive_pool_lifecycle =
            crate::write_owner::DrivePoolLifecycle::with_timer_park_sender(timer_park_tx);
        let calibration_store = open_calibration_store_for_config(config)?;
        let base_cfg = crate::write_owner::WriteOwnerConfig {
            index_path: index_path.clone(),
            report: report.clone(),
            policy,
            audit_dir: config.audit.dir.clone(),
            audit_fsync: config.audit.fsync,
            audit_append_lock: audit_append_lock.clone(),
            reservations: reservations.clone(),
            actor_library_serial: String::new(),
            library_snapshot: library_snapshot.clone(),
            snapshot_miss_alarm: config.drives.snapshot_miss_alarm,
            managed_library_serials: Arc::new(managed_library_serials),
            cleaning: config.cleaning.clone(),
            tape_io: config.tape_io.clone(),
            io_memory: Arc::clone(&io_memory),
            write_admissions: crate::write_owner::WriteAdmissionCoordinator::default(),
            checkpoint_journal_dir: config.journal.dir.join("checkpoints"),
            checkpoint_max_bytes: config.daemon.checkpoint_max_bytes,
            checkpoint_max_objects: config.daemon.checkpoint_max_objects,
            checkpoint_max_age_seconds: config.daemon.checkpoint_max_age_seconds,
            session_idle_seconds: config.daemon.default_idle_timeout_seconds,
            lifecycle: Some(drive_pool_lifecycle.clone()),
            calibration_store: calibration_store.clone(),
        };
        let mut drive_txs = HashMap::new();
        let mut changer_txs = HashMap::new();
        for (library_serial, library, opened_drives) in opened_libraries {
            let mut actor_cfg = base_cfg.clone();
            actor_cfg.actor_library_serial = library_serial.clone();
            for (bay_addr, drive) in opened_drives {
                let key = crate::drive_pool::DriveKey::new(library_serial.clone(), bay_addr);
                let tx = crate::write_owner::spawn_drive_actor(bay_addr, drive, actor_cfg.clone());
                drive_txs.insert(key, tx);
            }
            let changer_tx =
                crate::write_owner::spawn_changer_actor(library.into_changer(), actor_cfg);
            changer_txs.insert(library_serial, changer_tx);
        }
        let drive_pool = crate::write_owner::DrivePool::new_with_lifecycle(
            changer_txs,
            drive_txs,
            reservations.clone(),
            drive_pool_lifecycle,
        );
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
            calibration_store,
        );
        state.drive_pool = Some(drive_pool.clone());
        state.spool_dir = Some(Arc::new(spool_dir));
        state.spool_budget_bytes = Some(spool_budget_bytes);
        state.io_memory = io_memory;
        state.append_staging_mode = config.daemon.append_staging_mode;
        state.append_ring_bytes = config.daemon.append_ring_bytes;
        state.append_ring_high_pct = config.daemon.append_ring_high_pct;
        state.append_ring_low_pct = config.daemon.append_ring_low_pct;
        state.checkpoint_journal_dir = Arc::new(config.journal.dir.join("checkpoints"));
        state.checkpoint_max_bytes = config.daemon.checkpoint_max_bytes;
        state.checkpoint_max_objects = config.daemon.checkpoint_max_objects;
        state.checkpoint_max_age_seconds = config.daemon.checkpoint_max_age_seconds;
        state.default_library_serial = default_library_serial;
        state.library_snapshot = Some(library_snapshot);
        state.drive_idle_unload_seconds = config.daemon.drive_idle_unload_seconds;
        crate::mount::spawn_timer_idle_dismount_listener(state.clone(), timer_park_rx);
        state.reconcile_drive_catalog_from_report(config, &report)?;
        state.reconcile_clean_runs_from_report(&report)?;
        crate::mount::register_startup_seated_cartridges(&state, &report);
        crate::mount::spawn_startup_terminal_recoveries(state.clone());
        spawn_drive_collection_workers(
            index_path,
            report,
            config,
            drive_pool,
            Arc::clone(&state.audit_append_lock),
        );
        Ok(state)
    }

    /// Rewind, unload, and return idle seated cartridges before daemon exit.
    pub async fn shutdown_drive_pool(&self) -> Result<(), Status> {
        crate::mount::shutdown_drive_pool(self).await
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

    /// Return the read-plan service implementation (`PlanBatchRead`).
    pub fn read_plan_service(&self) -> ReadPlanApi {
        ReadPlanApi {
            state: self.clone(),
        }
    }

    /// Return the read-only append-log query service implementation.
    pub fn audit_service(&self) -> AuditApi {
        AuditApi {
            state: self.clone(),
        }
    }

    /// Return the library-inspection service implementation.
    pub fn library_service(&self) -> LibraryServiceApi {
        LibraryServiceApi {
            state: self.clone(),
        }
    }

    pub(crate) fn index(&self) -> Result<CatalogIndex, Status> {
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

    pub(crate) fn busy_drive_bays(&self, library_serial: &str) -> std::collections::HashSet<u16> {
        self.drive_pool
            .as_ref()
            .map(|pool| pool.busy_bays(library_serial))
            .unwrap_or_default()
    }

    pub(crate) fn operates_library(&self, library_serial: &str) -> bool {
        if let Some(pool) = self.drive_pool.as_ref() {
            return pool.operates_library(library_serial);
        }
        self.default_library_serial
            .as_deref()
            .is_some_and(|serial| serial.as_str() == library_serial)
    }

    /// What the drive catalog knows about the drive occupying a changer bay.
    ///
    /// Retired rows are deliberately included. A retired drive is still bolted
    /// into the library and still reported by the changer, so excluding it from
    /// the lookup does not make it disappear — it makes it appear as a row with
    /// no identity, which is strictly worse than reporting it as retired.
    /// Exclusion from *use* is enforced where it belongs, in mount resolution
    /// and in `get_actionable_drive_at`, both of which gate on `state`.
    pub(crate) fn resolve_drive_at_bay(
        &self,
        library_serial: &str,
        bay: u16,
    ) -> Result<BayResolution, Status> {
        let index = self.index()?;
        let mut claimants: Vec<remanence_state::DriveRecord> = index
            .list_drives(true, true)
            .map_err(status_from_state_error)?
            .into_iter()
            .filter(|drive| {
                drive.last_library_serial.as_deref() == Some(library_serial)
                    && drive.last_element_address == Some(i64::from(bay))
            })
            .collect();
        if claimants.is_empty() {
            return Ok(BayResolution::Uncatalogued);
        }
        // Prefer an active row, then the most recently seen: a bay reclaimed by
        // a replacement drive should report the replacement, not its predecessor.
        claimants.sort_by(|a, b| {
            let active = |d: &remanence_state::DriveRecord| d.state == "active";
            active(b)
                .cmp(&active(a))
                .then_with(|| b.last_seen_utc.cmp(&a.last_seen_utc))
        });
        let ambiguous = claimants.len() > 1;
        let chosen = claimants.swap_remove(0);
        if ambiguous {
            return Ok(BayResolution::Ambiguous(chosen));
        }
        if chosen.state == "retired" {
            return Ok(BayResolution::Retired(chosen));
        }
        Ok(BayResolution::Resolved(chosen))
    }

    pub(crate) fn library_is_managed(&self, library_serial: &str) -> bool {
        self.managed_library_serials.is_empty()
            || self.managed_library_serials.contains(library_serial.trim())
    }

    pub(crate) fn drive_counters(&self, drive_uuid: &[u8]) -> Arc<DriveByteCounters> {
        self.live_status
            .get_or_create_counters(self.daemon_epoch, drive_uuid)
    }

    pub(crate) fn record_drive_bytes(
        &self,
        drive_uuid: Option<&[u8]>,
        bytes: u64,
        kind: &'static str,
    ) {
        let Some(drive_uuid) = drive_uuid.filter(|drive_uuid| !drive_uuid.is_empty()) else {
            tracing::warn!(kind, bytes, "skipping byte accounting for unresolved drive");
            return;
        };
        let counters = self.drive_counters(drive_uuid);
        match kind {
            "read" => {
                counters.record_read_bytes(bytes);
            }
            "write" => {
                counters.record_write_bytes(bytes);
            }
            _ => unreachable!("byte-accounting kind must be read or write"),
        }
    }

    pub(crate) fn record_drive_read_bytes(&self, drive_uuid: Option<&[u8]>, bytes: u64) {
        self.record_drive_bytes(drive_uuid, bytes, "read");
    }

    #[cfg(test)]
    pub(crate) fn record_drive_write_bytes(&self, drive_uuid: Option<&[u8]>, bytes: u64) {
        self.record_drive_bytes(drive_uuid, bytes, "write");
    }

    pub(crate) async fn live_status_response(&self) -> Result<pb::GetLiveStatusResponse, Status> {
        self.live_status_response_inner(true)
    }

    pub(crate) async fn fresh_live_status_response(
        &self,
    ) -> Result<pb::GetLiveStatusResponse, Status> {
        self.live_status_response_inner(false)
    }

    pub(crate) fn live_status_response_inner(
        &self,
        allow_cached: bool,
    ) -> Result<pb::GetLiveStatusResponse, Status> {
        let snapshot_at = OffsetDateTime::now_utc();
        if allow_cached {
            if let Some(cached) = self
                .live_status
                .cache
                .read()
                .unwrap_or_else(|err| err.into_inner())
                .clone()
            {
                if snapshot_at - cached.0 < self.live_status.min_poll_interval {
                    let mut response = cached.1;
                    self.refresh_live_observations(&mut response);
                    response.drive_assignments = self.drive_assignments(&response.libraries)?;
                    return Ok(response);
                }
            }
        }

        let snapshot = self
            .current_library_snapshot()
            .ok_or_else(|| Status::not_found("library not found"))?;
        let index = self.index()?;
        let voltags = crate::library::voltag_uuid_map(&index)?;
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
            let busy_bays = self.busy_drive_bays(&library.serial);
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
                self.live_status
                    .observe_mount(library.serial.as_str(), drive);
                let bay = drive
                    .element_address
                    .ok_or_else(|| Status::internal("changer-projected drive has no bay"))
                    .and_then(|address| {
                        u16::try_from(address).map_err(|_| {
                            Status::invalid_argument("drive element address overflows u16")
                        })
                    })?;
                // Every arm is handled explicitly. The bug this replaces was a
                // missing `else`: an unresolved bay kept the zero-seeded row and
                // reported an empty identity as though it were a value.
                match self.resolve_drive_at_bay(library.serial.as_str(), bay)? {
                    BayResolution::Resolved(record) => {
                        self.enrich_live_drive(
                            drive,
                            &record,
                            active_clean_run_drive_uuids.contains(&record.drive_uuid),
                            open_session_by_drive.get(&record.drive_uuid),
                        );
                        drive.catalog_state = pb::DriveCatalogState::Cataloged as i32;
                    }
                    BayResolution::Retired(record) => {
                        // Identity and history are reported so the operator can
                        // see WHICH drive is retired in a bay that is still
                        // physically occupied. Selection paths gate on state, so
                        // reporting it here cannot cause it to be used.
                        self.enrich_live_drive(
                            drive,
                            &record,
                            active_clean_run_drive_uuids.contains(&record.drive_uuid),
                            open_session_by_drive.get(&record.drive_uuid),
                        );
                        drive.catalog_state = pb::DriveCatalogState::Retired as i32;
                    }
                    BayResolution::Ambiguous(record) => {
                        self.enrich_live_drive(
                            drive,
                            &record,
                            active_clean_run_drive_uuids.contains(&record.drive_uuid),
                            open_session_by_drive.get(&record.drive_uuid),
                        );
                        drive.catalog_state = pb::DriveCatalogState::Ambiguous as i32;
                    }
                    BayResolution::Uncatalogued => {
                        drive.catalog_state = pb::DriveCatalogState::Uncatalogued as i32;
                    }
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
        let drive_assignments = self.drive_assignments(&libraries)?;
        let response = pb::GetLiveStatusResponse {
            libraries,
            operations,
            alarms,
            snapshot_at_utc,
            daemon_epoch: self.daemon_epoch,
            drive_assignments,
        };
        *self
            .live_status
            .cache
            .write()
            .unwrap_or_else(|err| err.into_inner()) = Some((snapshot_at, response.clone()));
        Ok(response)
    }

    pub(crate) fn refresh_live_observations(&self, response: &mut pb::GetLiveStatusResponse) {
        for library_state in &mut response.libraries {
            let library_serial = library_state
                .library
                .as_ref()
                .map(|library| library.library_serial.as_str())
                .unwrap_or_default();
            for drive in &mut library_state.drives {
                self.live_status.observe_mount(library_serial, drive);
                self.enrich_live_counters(drive);
            }
        }
    }

    /// Project the live reservation atomics without making them a policy gate.
    pub(crate) fn drive_assignments(
        &self,
        libraries: &[pb::LibraryState],
    ) -> Result<Vec<pb::DriveAssignment>, Status> {
        let Some(drive_pool) = self.drive_pool.as_ref() else {
            return Ok(Vec::new());
        };
        let busy_drives = drive_pool.busy_drives();
        let sessions_by_drive = drive_pool.sessions_by_drive();
        let mut assignments = Vec::new();
        for library_state in libraries {
            let Some(library) = library_state.library.as_ref() else {
                continue;
            };
            if !drive_pool.operates_library(&library.library_serial) {
                continue;
            }
            for drive in &library_state.drives {
                let bay = drive
                    .element_address
                    .ok_or_else(|| Status::internal("changer-projected drive has no bay"))
                    .and_then(|address| {
                        u16::try_from(address).map_err(|_| {
                            Status::invalid_argument("drive element address overflows u16")
                        })
                    })?;
                let drive_key =
                    crate::drive_pool::DriveKey::new(library.library_serial.clone(), bay);
                let is_busy = busy_drives.contains(&drive_key);
                let session = if is_busy {
                    sessions_by_drive.get(&drive_key)
                } else {
                    None
                };
                assignments.push(pb::DriveAssignment {
                    library_serial: library.library_serial.clone(),
                    bay: u32::from(bay),
                    drive_uuid: drive.drive_uuid.clone(),
                    state: if is_busy {
                        pb::drive_assignment::State::DriveAssignmentStateActive as i32
                    } else {
                        pb::drive_assignment::State::DriveAssignmentStateIdle as i32
                    },
                    // Absent means idle, not "session zero".
                    current_session_id: session
                        .map(|(session_id, _)| session_id.as_bytes().to_vec()),
                    // Prefer the session's tape; fall back to whatever the
                    // changer saw seated. If neither knows, say so.
                    loaded_tape_uuid: session
                        .map(|(_, mounted)| mounted.tape_uuid.to_vec())
                        .or_else(|| drive.loaded_tape_uuid.clone()),
                });
            }
        }
        assignments.sort_by(|left, right| {
            (&left.library_serial, left.bay).cmp(&(&right.library_serial, right.bay))
        });
        Ok(assignments)
    }

    pub(crate) fn enrich_live_drive(
        &self,
        drive: &mut pb::Drive,
        record: &remanence_state::DriveRecord,
        cleaning_active: bool,
        open_session_id: Option<&Vec<u8>>,
    ) {
        let drive_uuid = record.drive_uuid.clone();
        drive.drive_uuid = Some(drive_uuid.clone());
        drive.cleaning_due = Some(if record.managed == "foreign" {
            "none".to_string()
        } else {
            record.cleaning_due.clone()
        });
        drive.fenced = Some(record.fenced);
        // Absent means no open session, which is not session zero.
        drive.session_id = open_session_id.cloned();
        drive.active_alert_names = if cleaning_active {
            vec!["cleaning".to_string()]
        } else {
            Vec::new()
        };
        self.enrich_live_counters(drive);
        if cleaning_active {
            drive.status = pb::drive::Status::DriveStatusCleaning as i32;
        } else if drive.fenced.unwrap_or(false) || record.fenced {
            drive.status = pb::drive::Status::DriveStatusFenced as i32;
        }
    }

    /// Fill in live counters and transfer telemetry, where they exist.
    ///
    /// Note what this function does NOT have: an `else`. When no counters are
    /// registered for a drive, every telemetry field is simply left as the
    /// projection set it. That used to be zero, so a drive nobody was measuring
    /// reported perfect numbers -- a 0us I/O gap, a flawless cadence -- and an
    /// operator had no way to tell a well-behaved idle drive from one that was
    /// never instrumented. The missing `else` was invisible precisely because
    /// the value it failed to write was already a plausible value.
    ///
    /// Now the projection leaves `None`, so the same missing branch reports
    /// "not measured", which is what was true all along.
    pub(crate) fn enrich_live_counters(&self, drive: &mut pb::Drive) {
        let Some(drive_uuid) = drive.drive_uuid.clone() else {
            return;
        };
        let counters = self
            .live_status
            .drive_counters
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .get(&drive_uuid)
            .cloned();
        drive.counter_epoch = Some(counters.as_ref().map_or_else(
            || LiveStatusState::counter_epoch(self.daemon_epoch, drive_uuid.as_slice()),
            |counters| counters.counter_epoch,
        ));
        if let Some(counters) = counters {
            drive.lifetime_read_bytes = Some(counters.read_bytes.load(Ordering::Relaxed));
            drive.lifetime_write_bytes = Some(counters.write_bytes.load(Ordering::Relaxed));
            drive.tape_io_staging_ring_buffers = Some(
                counters
                    .tape_io_staging_ring_buffers
                    .load(Ordering::Relaxed) as u32,
            );
            drive.tape_io_effective_batch_blocks = Some(
                counters
                    .tape_io_effective_batch_blocks
                    .load(Ordering::Relaxed) as u32,
            );
            drive.tape_io_gap_p50_us = Some(counters.tape_io_gap_p50_us.load(Ordering::Relaxed));
            drive.tape_io_gap_p95_us = Some(counters.tape_io_gap_p95_us.load(Ordering::Relaxed));
            drive.tape_io_gap_max_us = Some(counters.tape_io_gap_max_us.load(Ordering::Relaxed));
            drive.tape_io_ioctl_p50_us =
                Some(counters.tape_io_ioctl_p50_us.load(Ordering::Relaxed));
            drive.tape_io_ioctl_p95_us =
                Some(counters.tape_io_ioctl_p95_us.load(Ordering::Relaxed));
            drive.tape_io_ioctl_max_us =
                Some(counters.tape_io_ioctl_max_us.load(Ordering::Relaxed));
            drive.tape_io_cadence_us = Some(counters.tape_io_cadence_us.load(Ordering::Relaxed));
            drive.tape_io_effective_feed_bytes_per_second = Some(
                counters
                    .tape_io_effective_feed_bytes_per_second
                    .load(Ordering::Relaxed),
            );
            drive.tape_io_window_feed_bytes_per_second =
                Some(counters.window_feed_bytes_per_second());
        }
    }

    pub(crate) fn index_path(&self) -> PathBuf {
        self.index_path.as_ref().clone()
    }

    pub(crate) fn spool_dir(&self) -> Result<&Path, Status> {
        self.spool_dir
            .as_deref()
            .map(PathBuf::as_path)
            .ok_or_else(|| Status::unavailable("daemon has no write spool (read-only mode)"))
    }

    pub(crate) fn validate_spool_budget(&self, cap_bytes: u64) -> Result<(), Status> {
        let budget_bytes = self
            .spool_budget_bytes
            .ok_or_else(|| Status::unavailable("daemon has no write spool (read-only mode)"))?;
        if cap_bytes > budget_bytes {
            let spool_dir = self
                .spool_dir
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "(unconfigured)".to_string());
            return Err(Status::resource_exhausted(format!(
                "append spool request {cap_bytes} bytes exceeds effective daemon.spool_tmpfs_ram_budget {budget_bytes} bytes for {spool_dir}; overflow-to-disk is not implemented"
            )));
        }
        Ok(())
    }

    pub(crate) fn reserve_io_memory(
        &self,
        bytes: u64,
    ) -> Result<crate::io_memory::IoMemoryPermit, Status> {
        self.io_memory.try_reserve(bytes).ok_or_else(|| {
            Status::resource_exhausted(format!(
                "append spool growth of {bytes} bytes exceeds remaining daemon.io_memory_ceiling capacity"
            ))
        })
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

    pub(crate) fn reconcile_drive_catalog_from_report(
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

    pub(crate) fn reconcile_clean_runs_from_report(
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

    pub(crate) fn record_request_received(
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

    pub(crate) fn record_library_request_received(
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

    pub(crate) fn record_cancel_requested(
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

    pub(crate) fn record_operation_failed(
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
