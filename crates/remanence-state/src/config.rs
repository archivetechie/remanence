//! Operator configuration loading and validation.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{de, Deserialize, Deserializer};

use crate::error::StateError;

/// Default fixed tape block size for newly initialized tapes.
pub const DEFAULT_TAPE_BLOCK_SIZE_BYTES: u64 = 256 * 1024;

/// Default fixed ceiling shared by append spools and read reservoirs.
pub const DEFAULT_IO_MEMORY_CEILING_BYTES: u64 = 24 * 1024 * 1024 * 1024;
/// Default target size of one overlap append's receive ring.
pub const DEFAULT_APPEND_RING_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Default target size of one restore stream's read reservoir.
pub const DEFAULT_READ_RESERVOIR_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Default ranged-read device-position proof cadence.
pub const DEFAULT_RANGED_POSITION_CHECK_BYTES: u64 = 256 * 1024 * 1024;
/// Default delay before an idle library drive is rewound and unloaded.
pub const DEFAULT_DRIVE_IDLE_UNLOAD_SECONDS: u64 = 300;
/// Default byte threshold for a batched filemark checkpoint.
pub const DEFAULT_CHECKPOINT_MAX_BYTES: u64 = 32 * 1024 * 1024 * 1024;
/// Default object threshold for a batched filemark checkpoint.
pub const DEFAULT_CHECKPOINT_MAX_OBJECTS: u64 = 200;
/// Default age threshold for a batched filemark checkpoint.
pub const DEFAULT_CHECKPOINT_MAX_AGE_SECONDS: u64 = 300;

/// Top-level Remanence daemon configuration.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RemConfig {
    /// Daemon-wide settings.
    pub daemon: DaemonConfig,
    /// Allowed tape libraries.
    #[serde(default)]
    pub libraries: Vec<LibraryConfig>,
    /// Operator-defined tape eligibility pools.
    #[serde(default)]
    pub tape_pools: Vec<TapePoolConfig>,
    /// Barcode-prefix rules that derive tape pool membership from voltags.
    #[serde(default)]
    pub tape_pool_rules: Vec<TapePoolRuleConfig>,
    /// Drive stewardship collection settings.
    #[serde(default)]
    pub drives: DrivesConfig,
    /// Cleaning policy settings.
    #[serde(default)]
    pub cleaning: CleaningConfig,
    /// Live-status serving settings.
    #[serde(default)]
    pub livestatus: LiveStatusConfig,
    /// Tape I/O batching and position-proof settings.
    #[serde(default)]
    pub tape_io: TapeIoConfig,
    /// Layer 3c journal settings.
    pub journal: JournalConfig,
    /// Layer 4 audit-log settings.
    pub audit: AuditConfig,
    /// Rebuildable SQLite index settings.
    pub index: IndexConfig,
    /// Rebuildable tape-catalog cache settings.
    pub cache: CacheConfig,
}

/// Daemon-wide settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Root directory for mutable daemon state.
    pub state_dir: PathBuf,
    /// Directory for pre-commit append spool files. Absent → `<state_dir>/spool`.
    #[serde(default)]
    pub spool_dir: Option<PathBuf>,
    /// Required operator acknowledgment when `spool_dir` is RAM-backed tmpfs.
    #[serde(default, deserialize_with = "deserialize_optional_byte_size")]
    pub spool_tmpfs_ram_budget: Option<u64>,
    /// Fixed aggregate budget for spool and read-reservoir memory.
    #[serde(
        default = "default_io_memory_ceiling",
        deserialize_with = "deserialize_byte_size"
    )]
    pub io_memory_ceiling: u64,
    /// Append receive strategy. The v0.1 rollout remains opt-in.
    #[serde(default)]
    pub append_staging_mode: AppendStagingMode,
    /// Target byte capacity of one overlap append receive ring.
    #[serde(
        default = "default_append_ring_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub append_ring_bytes: u64,
    /// Start or resume tape submission at this ring occupancy percentage.
    #[serde(default = "default_append_ring_high_pct")]
    pub append_ring_high_pct: u8,
    /// Pause tape submission at this ring occupancy percentage.
    #[serde(default = "default_append_ring_low_pct")]
    pub append_ring_low_pct: u8,
    /// Maximum pending logical bytes before a batched checkpoint barrier.
    #[serde(
        default = "default_checkpoint_max_bytes",
        deserialize_with = "deserialize_byte_size"
    )]
    pub checkpoint_max_bytes: u64,
    /// Maximum pending objects before a batched checkpoint barrier.
    #[serde(default = "default_checkpoint_max_objects")]
    pub checkpoint_max_objects: u64,
    /// Maximum age of the oldest pending object before a barrier is requested.
    #[serde(default = "default_checkpoint_max_age_seconds")]
    pub checkpoint_max_age_seconds: u64,
    /// Default idle timeout for sessions.
    pub default_idle_timeout_seconds: u64,
    /// Delay before a seated cartridge in an idle drive is returned home.
    /// Zero keeps idle cartridges seated until eviction or daemon shutdown.
    #[serde(default = "default_drive_idle_unload_seconds")]
    pub drive_idle_unload_seconds: u64,
    /// Whether state-changing operations must be rejected.
    #[serde(default)]
    pub read_only: bool,
    /// Unix-domain socket the daemon listens on (dev transport). Absent →
    /// `<state_dir>/rem.sock`.
    #[serde(default)]
    pub socket_path: Option<PathBuf>,
    /// TCP listen address for the mTLS endpoint, e.g. "0.0.0.0:8443".
    /// Requires `tls`. Absent means Unix socket only.
    #[serde(default)]
    pub listen: Option<String>,
    /// Mutual-TLS material for the TCP listener. Requires `listen`.
    #[serde(default)]
    pub tls: Option<DaemonTlsConfig>,
}

/// Daemon policy for Layer 5 append receive staging.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AppendStagingMode {
    /// Receive the complete object into the legacy spool before tape writing.
    #[default]
    Serial,
    /// Use a bounded live receive ring when the caller supplies every proof.
    Overlap,
}

impl DaemonConfig {
    /// Resolve the listen socket: explicit `socket_path`, else `<state_dir>/rem.sock`.
    pub fn socket_path_or_default(&self) -> PathBuf {
        self.socket_path
            .clone()
            .unwrap_or_else(|| self.state_dir.join("rem.sock"))
    }

    /// Resolve the append spool directory: explicit `spool_dir`, else `<state_dir>/spool`.
    pub fn spool_dir_or_default(&self) -> PathBuf {
        self.spool_dir
            .clone()
            .unwrap_or_else(|| self.state_dir.join("spool"))
    }
}

/// Server-side mutual-TLS material for the daemon's TCP listener.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DaemonTlsConfig {
    /// Server identity certificate PEM.
    pub cert: PathBuf,
    /// Server private key PEM.
    pub key: PathBuf,
    /// CA PEM whose signature a client certificate must carry.
    pub client_ca: PathBuf,
}

/// Per-library operator allowlist entry.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LibraryConfig {
    /// Library serial number.
    pub serial: String,
    /// Whether derived drive identity is allowed for this explicit library.
    #[serde(default)]
    pub allow_derived_drive_identity: bool,
}

/// Operator-defined pool for tape eligibility and Sutradhara placement.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TapePoolConfig {
    /// Stable daemon-local pool id.
    pub id: String,
    /// Optional human-readable label.
    pub display_name: Option<String>,
    /// Optional copy segregation axis, such as `copy-a`.
    pub copy_class: Option<String>,
    /// Optional content segregation axis, such as `camera`.
    pub content_class: Option<String>,
    /// Within-pool tape-selection policy.
    #[serde(default)]
    pub selection_policy: PoolSelectionPolicyName,
    /// Fill target. Tapes are sealed when actual used bytes reach this fraction.
    #[serde(default = "default_watermark_low")]
    pub watermark_low: f64,
    /// Usable capacity cap, below physical end-of-media.
    #[serde(default = "default_watermark_high")]
    pub watermark_high: f64,
    /// Fixed tape block size to record when initializing fresh tapes in this pool.
    #[serde(
        default = "default_tape_block_size",
        rename = "block_size",
        deserialize_with = "deserialize_byte_size"
    )]
    pub block_size_bytes: u64,
    /// Sutradhara's declared minimum object/bundle floor in bytes.
    #[serde(
        default,
        rename = "min_object_size",
        deserialize_with = "deserialize_byte_size"
    )]
    pub min_object_size_bytes: u64,
}

/// Configured within-pool tape-selection policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PoolSelectionPolicyName {
    /// Default two-tier complete-or-fill policy.
    #[default]
    CompleteOrFill,
    /// Compatibility first-fit-by-barcode policy.
    FillOldest,
}

impl PoolSelectionPolicyName {
    /// Return the stable config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CompleteOrFill => "complete-or-fill",
            Self::FillOldest => "fill-oldest",
        }
    }
}

impl<'de> Deserialize<'de> for PoolSelectionPolicyName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim() {
            "complete-or-fill" => Ok(Self::CompleteOrFill),
            "fill-oldest" => Ok(Self::FillOldest),
            other => Err(de::Error::custom(format!(
                "unknown selection_policy {other:?}; expected complete-or-fill or fill-oldest"
            ))),
        }
    }
}

/// Barcode-prefix rule that derives one tape's pool from its voltag.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TapePoolRuleConfig {
    /// Barcode prefix to match. Longest matching prefix wins.
    pub prefix: String,
    /// Pool id from `[[tape_pools]]`.
    pub pool_id: String,
}

/// Drive stewardship collection settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct DrivesConfig {
    /// Library serials Remanence may actively manage. Empty means daemon-operated libraries.
    pub managed_libraries: Vec<String>,
    /// Cadence for foreign-drive error-counter polling.
    pub foreign_counter_poll: String,
    /// Opt-in to foreign TapeAlert page reads.
    pub foreign_tapealert: bool,
    /// Managed-drive liveness heartbeat cadence.
    pub heartbeat: String,
    /// Consecutive missed snapshots before raising an alarm.
    pub snapshot_miss_alarm: u32,
}

impl Default for DrivesConfig {
    fn default() -> Self {
        Self {
            managed_libraries: Vec::new(),
            foreign_counter_poll: "60m".to_string(),
            foreign_tapealert: false,
            heartbeat: "1h".to_string(),
            snapshot_miss_alarm: 3,
        }
    }
}

/// Cleaning policy settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct CleaningConfig {
    /// Whether automatic cleaning is enabled.
    pub auto: bool,
    /// Voltag prefixes considered cleaning cartridges.
    pub voltag_prefixes: Vec<String>,
    /// Warning threshold for cartridge use count.
    pub use_warn: u32,
    /// Maximum cleaning run duration.
    pub complete_timeout: String,
    /// Minimum plausible completed cleaning-cycle duration.
    pub min_cycle_duration: String,
    /// Minimum interval between automatic cleans for one drive.
    pub min_interval: String,
    /// Weekly automatic-cleaning cap per drive.
    pub weekly_cap: u32,
}

impl Default for CleaningConfig {
    fn default() -> Self {
        Self {
            auto: true,
            voltag_prefixes: vec!["CLN".to_string()],
            use_warn: 45,
            complete_timeout: "10m".to_string(),
            min_cycle_duration: "60s".to_string(),
            min_interval: "12h".to_string(),
            weekly_cap: 4,
        }
    }
}

/// Live-status serving settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct LiveStatusConfig {
    /// Minimum poll interval enforced by the daemon.
    pub min_poll_interval: String,
    /// Foreign changer inventory poll cadence while live-status clients are active.
    pub foreign_changer_poll: String,
    /// Recency lease defining an active live-status consumer.
    pub foreign_poll_lease: String,
}

impl Default for LiveStatusConfig {
    fn default() -> Self {
        Self {
            min_poll_interval: "250ms".to_string(),
            foreign_changer_poll: "60s".to_string(),
            foreign_poll_lease: "5m".to_string(),
        }
    }
}

/// Tape I/O batching and position-proof settings.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct TapeIoConfig {
    /// Number of page-aligned buffers in each active drive's staging ring.
    pub staging_ring_buffers: u32,
    /// Requested fixed records per WRITE(6), before sg/HBA clamping.
    pub write_batch_blocks: u32,
    /// Requested fixed records per READ(6), before sg/HBA clamping.
    pub read_batch_blocks: u32,
    /// Drift tripwire cadence in bytes. Zero disables mid-stream checks.
    #[serde(deserialize_with = "deserialize_byte_size")]
    pub position_check_bytes: u64,
    /// Target byte capacity of one restore stream's host-RAM reservoir.
    #[serde(deserialize_with = "deserialize_byte_size")]
    pub read_reservoir_bytes: u64,
    /// Stop issuing reads at this percentage of effective reservoir capacity.
    pub read_reservoir_high_pct: u8,
    /// Resume issuing reads at this percentage of effective reservoir capacity.
    pub read_reservoir_low_pct: u8,
    /// Device-position proof cadence for hash-less ranged reads.
    #[serde(deserialize_with = "deserialize_byte_size")]
    pub position_check_bytes_ranged: u64,
}

impl Default for TapeIoConfig {
    fn default() -> Self {
        Self {
            staging_ring_buffers: remanence_library::DEFAULT_TAPE_IO_STAGING_RING_BUFFERS,
            write_batch_blocks: remanence_library::DEFAULT_TAPE_IO_BATCH_BLOCKS,
            read_batch_blocks: remanence_library::DEFAULT_TAPE_IO_BATCH_BLOCKS,
            position_check_bytes: remanence_library::DEFAULT_TAPE_IO_POSITION_CHECK_BYTES,
            read_reservoir_bytes: DEFAULT_READ_RESERVOIR_BYTES,
            read_reservoir_high_pct: 90,
            read_reservoir_low_pct: 25,
            position_check_bytes_ranged: DEFAULT_RANGED_POSITION_CHECK_BYTES,
        }
    }
}

const fn default_io_memory_ceiling() -> u64 {
    DEFAULT_IO_MEMORY_CEILING_BYTES
}

const fn default_append_ring_bytes() -> u64 {
    DEFAULT_APPEND_RING_BYTES
}

const fn default_append_ring_high_pct() -> u8 {
    90
}

const fn default_append_ring_low_pct() -> u8 {
    25
}

const fn default_drive_idle_unload_seconds() -> u64 {
    DEFAULT_DRIVE_IDLE_UNLOAD_SECONDS
}

const fn default_checkpoint_max_bytes() -> u64 {
    DEFAULT_CHECKPOINT_MAX_BYTES
}

const fn default_checkpoint_max_objects() -> u64 {
    DEFAULT_CHECKPOINT_MAX_OBJECTS
}

const fn default_checkpoint_max_age_seconds() -> u64 {
    DEFAULT_CHECKPOINT_MAX_AGE_SECONDS
}

/// Layer 3c journal configuration.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JournalConfig {
    /// Directory containing one `.remjournal` file per tape.
    pub dir: PathBuf,
    /// Whether startup rejects untrusted flush volumes.
    #[serde(default = "default_require_trusted_volume")]
    pub require_trusted_volume: bool,
}

/// Audit-log configuration.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    /// Directory containing daily `.remaudit` segments.
    pub dir: PathBuf,
    /// Whether appends are fsynced before returning.
    #[serde(default = "default_true")]
    pub fsync: bool,
    /// Wall-clock forward jump tolerance before an audit warning is emitted.
    #[serde(default = "default_clock_forward_tolerance_seconds")]
    pub clock_forward_tolerance_seconds: u64,
}

/// SQLite projection configuration.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    /// Path to the rebuildable SQLite index file.
    pub sqlite_path: PathBuf,
}

/// Rebuildable cache configuration.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    /// Directory containing per-tape catalog cache files.
    pub tape_catalog_dir: PathBuf,
}

fn default_require_trusted_volume() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_clock_forward_tolerance_seconds() -> u64 {
    300
}

fn default_watermark_low() -> f64 {
    0.92
}

fn default_watermark_high() -> f64 {
    0.97
}

fn default_tape_block_size() -> u64 {
    DEFAULT_TAPE_BLOCK_SIZE_BYTES
}

/// Load and validate a TOML configuration file.
pub fn load_config(path: impl AsRef<Path>) -> Result<RemConfig, StateError> {
    let path = path.as_ref();
    let text =
        fs::read_to_string(path).map_err(|err| StateError::io_at("read config", path, err))?;
    parse_config_toml(&text)
}

/// Parse and validate a TOML configuration string.
pub fn parse_config_toml(text: &str) -> Result<RemConfig, StateError> {
    let document: toml::Value =
        toml::from_str(text).map_err(|err| StateError::ConfigInvalid(err.to_string()))?;
    if document
        .get("daemon")
        .and_then(toml::Value::as_table)
        .is_some_and(|daemon| daemon.contains_key("checkpoint_mode"))
    {
        return Err(StateError::ConfigInvalid(
            "checkpoint_mode was removed 2026-07-21; batched is the only mode".to_string(),
        ));
    }
    if let Some(tape_io) = document.get("tape_io").and_then(toml::Value::as_table) {
        for removed in ["pipelined_submission", "legacy_single_block"] {
            if tape_io.contains_key(removed) {
                return Err(StateError::ConfigInvalid(format!(
                    "tape_io.{removed} was removed; the pipelined fixed-block path is now the only tape I/O path"
                )));
            }
        }
    }
    let config: RemConfig =
        toml::from_str(text).map_err(|err| StateError::ConfigInvalid(err.to_string()))?;
    validate_config(&config)?;
    Ok(config)
}

/// Validate a parsed configuration.
pub fn validate_config(config: &RemConfig) -> Result<(), StateError> {
    require_absolute("daemon.state_dir", &config.daemon.state_dir)?;
    if let Some(spool_dir) = &config.daemon.spool_dir {
        require_absolute("daemon.spool_dir", spool_dir)?;
    }
    if let Some(socket_path) = &config.daemon.socket_path {
        require_absolute("daemon.socket_path", socket_path)?;
    }
    match (&config.daemon.listen, &config.daemon.tls) {
        (Some(listen), Some(tls)) => {
            listen.parse::<std::net::SocketAddr>().map_err(|_| {
                StateError::ConfigInvalid(format!(
                    "daemon.listen {listen:?} must be a valid socket address (e.g. 0.0.0.0:8443)"
                ))
            })?;
            require_absolute("daemon.tls.cert", &tls.cert)?;
            require_absolute("daemon.tls.key", &tls.key)?;
            require_absolute("daemon.tls.client_ca", &tls.client_ca)?;
        }
        (None, None) => {}
        _ => {
            return Err(StateError::ConfigInvalid(
                "daemon.listen and daemon.tls must be set together".to_string(),
            ));
        }
    }
    require_absolute("journal.dir", &config.journal.dir)?;
    require_absolute("audit.dir", &config.audit.dir)?;
    require_absolute("index.sqlite_path", &config.index.sqlite_path)?;
    require_absolute("cache.tape_catalog_dir", &config.cache.tape_catalog_dir)?;

    if config.daemon.default_idle_timeout_seconds == 0 {
        return Err(StateError::ConfigInvalid(
            "daemon.default_idle_timeout_seconds must be non-zero".to_string(),
        ));
    }
    if config.daemon.spool_tmpfs_ram_budget == Some(0) {
        return Err(StateError::ConfigInvalid(
            "daemon.spool_tmpfs_ram_budget must be non-zero when set".to_string(),
        ));
    }
    if config.daemon.io_memory_ceiling == 0 {
        return Err(StateError::ConfigInvalid(
            "daemon.io_memory_ceiling must be non-zero".to_string(),
        ));
    }
    if config.daemon.append_ring_bytes == 0 {
        return Err(StateError::ConfigInvalid(
            "daemon.append_ring_bytes must be non-zero".to_string(),
        ));
    }
    if config.daemon.append_ring_low_pct == 0
        || config.daemon.append_ring_low_pct >= config.daemon.append_ring_high_pct
        || config.daemon.append_ring_high_pct > 100
    {
        return Err(StateError::ConfigInvalid(
            "daemon append ring watermarks require 0 < low < high <= 100".to_string(),
        ));
    }
    if config.daemon.checkpoint_max_bytes == 0 {
        return Err(StateError::ConfigInvalid(
            "daemon.checkpoint_max_bytes must be non-zero".to_string(),
        ));
    }
    if config.daemon.checkpoint_max_objects == 0 {
        return Err(StateError::ConfigInvalid(
            "daemon.checkpoint_max_objects must be non-zero".to_string(),
        ));
    }
    if config.daemon.checkpoint_max_age_seconds == 0 {
        return Err(StateError::ConfigInvalid(
            "daemon.checkpoint_max_age_seconds must be non-zero".to_string(),
        ));
    }
    if config
        .daemon
        .spool_tmpfs_ram_budget
        .is_some_and(|budget| budget > config.daemon.io_memory_ceiling)
    {
        return Err(StateError::ConfigInvalid(
            "daemon.spool_tmpfs_ram_budget must not exceed daemon.io_memory_ceiling".to_string(),
        ));
    }
    if config.audit.clock_forward_tolerance_seconds > i64::MAX as u64 {
        return Err(StateError::ConfigInvalid(
            "audit.clock_forward_tolerance_seconds is too large".to_string(),
        ));
    }
    if config.tape_io.write_batch_blocks == 0 {
        return Err(StateError::ConfigInvalid(
            "tape_io.write_batch_blocks must be non-zero".to_string(),
        ));
    }
    if config.tape_io.read_batch_blocks == 0 {
        return Err(StateError::ConfigInvalid(
            "tape_io.read_batch_blocks must be non-zero".to_string(),
        ));
    }
    if config.tape_io.read_reservoir_bytes == 0
        || config.tape_io.read_reservoir_bytes > config.daemon.io_memory_ceiling
    {
        return Err(StateError::ConfigInvalid(
            "tape_io.read_reservoir_bytes must be non-zero and not exceed daemon.io_memory_ceiling"
                .to_string(),
        ));
    }
    if config.tape_io.read_reservoir_low_pct == 0
        || config.tape_io.read_reservoir_low_pct >= config.tape_io.read_reservoir_high_pct
        || config.tape_io.read_reservoir_high_pct > 100
    {
        return Err(StateError::ConfigInvalid(
            "tape_io reservoir watermarks require 0 < read_reservoir_low_pct < read_reservoir_high_pct <= 100"
                .to_string(),
        ));
    }
    if config.tape_io.position_check_bytes_ranged == 0 {
        return Err(StateError::ConfigInvalid(
            "tape_io.position_check_bytes_ranged must be non-zero".to_string(),
        ));
    }
    if !(remanence_library::MIN_TAPE_IO_STAGING_RING_BUFFERS
        ..=remanence_library::MAX_TAPE_IO_STAGING_RING_BUFFERS)
        .contains(&config.tape_io.staging_ring_buffers)
    {
        return Err(StateError::ConfigInvalid(format!(
            "tape_io.staging_ring_buffers must be in {}..={}",
            remanence_library::MIN_TAPE_IO_STAGING_RING_BUFFERS,
            remanence_library::MAX_TAPE_IO_STAGING_RING_BUFFERS,
        )));
    }

    let mut serials = HashSet::new();
    for library in &config.libraries {
        let serial = library.serial.trim();
        if serial.is_empty() {
            return Err(StateError::ConfigInvalid(
                "library serial must not be empty".to_string(),
            ));
        }
        if !serials.insert(serial.to_string()) {
            return Err(StateError::ConfigInvalid(format!(
                "duplicate library serial {serial}"
            )));
        }
    }

    let mut pool_ids = HashSet::new();
    for pool in &config.tape_pools {
        let pool_id = validate_pool_id(pool.id.as_str())?;
        validate_block_size(pool.block_size_bytes)
            .map_err(|error| StateError::ConfigInvalid(format!("tape pool {pool_id} {error}")))?;
        validate_tape_pool_selection_config(pool)?;
        if !pool_ids.insert(pool_id.to_string()) {
            return Err(StateError::ConfigInvalid(format!(
                "duplicate tape pool id {pool_id}"
            )));
        }
    }
    validate_tape_pool_rules(&config.tape_pool_rules, &pool_ids)?;

    validate_trusted_volume_paths(config)?;

    Ok(())
}

/// Derive a tape pool from a barcode using longest-prefix matching.
///
/// Prefix and voltag matching is ASCII case-insensitive. Validation rejects
/// duplicate normalized prefixes, so ties are not meaningful for valid config.
pub fn derive_tape_pool_from_voltag<'a>(
    voltag: &str,
    rules: &'a [TapePoolRuleConfig],
) -> Option<&'a str> {
    let voltag = normalize_rule_match_text(voltag)?;
    rules
        .iter()
        .filter_map(|rule| {
            let prefix = normalize_rule_match_text(&rule.prefix)?;
            voltag
                .starts_with(prefix.as_str())
                .then_some((prefix.len(), rule.pool_id.as_str()))
        })
        .max_by_key(|(prefix_len, _)| *prefix_len)
        .map(|(_, pool_id)| pool_id)
}

/// Validate a configured fixed tape block size.
pub fn validate_block_size(block_size_bytes: u64) -> Result<(), String> {
    const ALLOWED: [u64; 3] = [256 * 1024, 512 * 1024, 1024 * 1024];
    if !ALLOWED.contains(&block_size_bytes) {
        return Err("block_size must be one of 256KiB, 512KiB, or 1MiB".to_string());
    }
    Ok(())
}

/// Validate the static, capacity-independent selection settings for one pool.
pub fn validate_tape_pool_selection_config(pool: &TapePoolConfig) -> Result<(), StateError> {
    validate_watermark("watermark_low", pool.watermark_low)?;
    validate_watermark("watermark_high", pool.watermark_high)?;
    if pool.watermark_low.partial_cmp(&pool.watermark_high) != Some(std::cmp::Ordering::Less) {
        return Err(StateError::ConfigInvalid(format!(
            "tape pool {} requires 0 < watermark_low < watermark_high <= 1",
            pool.id
        )));
    }
    Ok(())
}

fn validate_tape_pool_rules(
    rules: &[TapePoolRuleConfig],
    pool_ids: &HashSet<String>,
) -> Result<(), StateError> {
    let mut prefixes = HashMap::new();
    for rule in rules {
        let prefix = validate_tape_pool_rule_prefix(&rule.prefix)?;
        let pool_id = validate_pool_id(rule.pool_id.as_str())?;
        if !pool_ids.contains(pool_id) {
            return Err(StateError::ConfigInvalid(format!(
                "tape pool rule prefix {prefix:?} references unknown pool id {pool_id}"
            )));
        }
        if let Some(existing_pool) = prefixes.insert(prefix.to_string(), pool_id.to_string()) {
            return Err(StateError::ConfigInvalid(format!(
                "ambiguous tape pool rule prefix {prefix:?}: maps to both {existing_pool} and {pool_id}"
            )));
        }
    }
    Ok(())
}

fn validate_tape_pool_rule_prefix(value: &str) -> Result<String, StateError> {
    let prefix = normalize_rule_match_text(value).ok_or_else(|| {
        StateError::ConfigInvalid("tape pool rule prefix must be non-empty ASCII".to_string())
    })?;
    if !prefix.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(StateError::ConfigInvalid(format!(
            "tape pool rule prefix {value:?} must use only ASCII letters and digits"
        )));
    }
    Ok(prefix)
}

fn normalize_rule_match_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.is_ascii() {
        return None;
    }
    Some(trimmed.to_ascii_uppercase())
}

/// Validate the watermark-band invariant once the pool's tape capacity is known.
pub fn validate_tape_pool_capacity_invariant(
    pool: &TapePoolConfig,
    capacity_bytes: u64,
) -> Result<(), StateError> {
    let low_bytes = watermark_floor_bytes(capacity_bytes, pool.watermark_low)?;
    let high_bytes = watermark_floor_bytes(capacity_bytes, pool.watermark_high)?;
    let band_bytes = high_bytes.saturating_sub(low_bytes);
    if band_bytes < pool.min_object_size_bytes {
        return Err(StateError::ConfigInvalid(format!(
            "tape pool {} watermark band {band_bytes} bytes is smaller than min_object_size {} bytes for capacity {capacity_bytes}",
            pool.id, pool.min_object_size_bytes
        )));
    }
    Ok(())
}

/// Convert a capacity fraction into the byte threshold used by selection.
pub fn watermark_floor_bytes(capacity_bytes: u64, watermark: f64) -> Result<u64, StateError> {
    validate_watermark("watermark", watermark)?;
    Ok(((capacity_bytes as f64) * watermark).floor() as u64)
}

/// Validate configured journal and audit volumes when trust checks are enabled.
pub fn validate_trusted_volume_paths(config: &RemConfig) -> Result<(), StateError> {
    if config.journal.require_trusted_volume {
        for path in trusted_volume_paths(config) {
            validate_trusted_path(path)?;
        }
    }
    Ok(())
}

fn trusted_volume_paths(config: &RemConfig) -> Vec<&Path> {
    let mut paths = vec![
        config.daemon.state_dir.as_path(),
        config.journal.dir.as_path(),
        config.audit.dir.as_path(),
        config.index.sqlite_path.as_path(),
        config.cache.tape_catalog_dir.as_path(),
    ];
    if let Some(socket_path) = &config.daemon.socket_path {
        paths.push(socket_path.as_path());
    }
    paths
}

fn require_absolute(name: &str, path: &Path) -> Result<(), StateError> {
    if !path.is_absolute() {
        return Err(StateError::ConfigInvalid(format!(
            "{name} must be an absolute path: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_pool_id(value: &str) -> Result<&str, StateError> {
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
    Ok(pool_id)
}

fn validate_watermark(field: &str, value: f64) -> Result<(), StateError> {
    if value.is_finite() && value > 0.0 && value <= 1.0 {
        Ok(())
    } else {
        Err(StateError::ConfigInvalid(format!(
            "{field} must be finite and satisfy 0 < {field} <= 1"
        )))
    }
}

fn deserialize_byte_size<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct ByteSizeVisitor;

    impl de::Visitor<'_> for ByteSizeVisitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a byte count integer or a string like \"2GiB\"")
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
            Ok(value)
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            u64::try_from(value).map_err(|_| E::custom("byte size must not be negative"))
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            parse_byte_size(value).map_err(E::custom)
        }
    }

    deserializer.deserialize_any(ByteSizeVisitor)
}

fn deserialize_optional_byte_size<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct ByteSizeValue(#[serde(deserialize_with = "deserialize_byte_size")] u64);

    Ok(Option::<ByteSizeValue>::deserialize(deserializer)?.map(|value| value.0))
}

fn parse_byte_size(value: &str) -> Result<u64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("byte size must not be empty".to_string());
    }
    let split_at = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    let (digits, unit) = value.split_at(split_at);
    if digits.is_empty() {
        return Err(format!("byte size {value:?} must start with digits"));
    }
    let amount = digits
        .parse::<u64>()
        .map_err(|err| format!("byte size {value:?} has invalid number: {err}"))?;
    let multiplier = match unit.trim() {
        "" | "B" | "b" => 1u64,
        "KiB" | "K" | "KB" => 1024,
        "MiB" | "M" | "MB" => 1024u64.pow(2),
        "GiB" | "G" | "GB" => 1024u64.pow(3),
        "TiB" | "T" | "TB" => 1024u64.pow(4),
        "PiB" | "P" | "PB" => 1024u64.pow(5),
        other => return Err(format!("unsupported byte-size unit {other:?}")),
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(|| format!("byte size {value:?} overflows u64"))
}

fn validate_trusted_path(path: &Path) -> Result<(), StateError> {
    let probe = nearest_existing_ancestor(path)?;
    reject_untrusted_volume(path, &probe)
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf, StateError> {
    let mut current = path;
    loop {
        if current.exists() {
            return Ok(current.to_path_buf());
        }
        current = current.parent().ok_or_else(|| {
            StateError::ConfigInvalid(format!(
                "no existing ancestor for trusted-volume check: {}",
                path.display()
            ))
        })?;
    }
}

#[cfg(target_os = "linux")]
fn reject_untrusted_volume(configured: &Path, probe: &Path) -> Result<(), StateError> {
    let stats = nix::sys::statfs::statfs(probe).map_err(|err| {
        StateError::io_at(
            "statfs trusted-volume probe",
            probe,
            std::io::Error::from(err),
        )
    })?;
    let fs_type = stats.filesystem_type().0 as u64;
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    const NFS_SUPER_MAGIC: u64 = 0x6969;
    const SMB_SUPER_MAGIC: u64 = 0x517B;
    const CIFS_MAGIC_NUMBER: u64 = 0xFF53_4D42;
    const OVERLAYFS_SUPER_MAGIC: u64 = 0x794C_7630;
    const RAMFS_MAGIC: u64 = 0x8584_58F6;

    let kind = match fs_type {
        TMPFS_MAGIC => Some("tmpfs"),
        NFS_SUPER_MAGIC => Some("nfs"),
        SMB_SUPER_MAGIC => Some("smb"),
        CIFS_MAGIC_NUMBER => Some("cifs"),
        OVERLAYFS_SUPER_MAGIC => Some("overlayfs"),
        RAMFS_MAGIC => Some("ramfs"),
        _ => None,
    };

    if let Some(kind) = kind {
        return Err(StateError::UntrustedStateVolume(format!(
            "{} is on {kind} via {}",
            configured.display(),
            probe.display()
        )));
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn reject_untrusted_volume(_configured: &Path, _probe: &Path) -> Result<(), StateError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> String {
        let root = std::env::temp_dir().join("remanence-state-config-test");
        format!(
            r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
read_only = false

[[libraries]]
serial = "LIB001"
allow_derived_drive_identity = false

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

    fn with_daemon_lines(extra: &str) -> String {
        valid_config().replace(
            "read_only = false\n",
            &format!("read_only = false\n{extra}"),
        )
    }

    #[test]
    fn rejects_duplicate_library_serials() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[libraries]]
serial = "LIB001"
"#,
        );

        let err = parse_config_toml(&text).expect_err("duplicate serial must fail");
        assert!(err.to_string().contains("duplicate library serial"));
    }

    #[test]
    fn rejects_duplicate_tape_pool_ids() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.copy-a"

[[tape_pools]]
id = "camera.copy-a"
"#,
        );

        let err = parse_config_toml(&text).expect_err("duplicate pool must fail");
        assert!(err.to_string().contains("duplicate tape pool id"));
    }

    #[test]
    fn rejects_invalid_tape_pool_ids() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera copy a"
"#,
        );

        let err = parse_config_toml(&text).expect_err("invalid pool must fail");
        assert!(err.to_string().contains("tape pool id"));
    }

    #[test]
    fn rejects_unknown_keys() {
        let text = valid_config().replace("[daemon]\n", "[daemon]\nunsupported_setting = true\n");

        let err = parse_config_toml(&text).expect_err("unknown key must fail");
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn daemon_socket_path_defaults_and_parses() {
        let config = parse_config_toml(&valid_config()).expect("valid config");
        assert_eq!(config.daemon.socket_path, None);
        assert_eq!(
            config.daemon.drive_idle_unload_seconds,
            DEFAULT_DRIVE_IDLE_UNLOAD_SECONDS
        );
        assert_eq!(
            config.daemon.socket_path_or_default(),
            config.daemon.state_dir.join("rem.sock")
        );

        let text = valid_config().replace(
            "default_idle_timeout_seconds = 1800",
            "default_idle_timeout_seconds = 1800\nsocket_path = \"/run/rem/rem.sock\"",
        );
        let config = parse_config_toml(&text).expect("valid config with socket_path");
        assert_eq!(
            config.daemon.socket_path_or_default(),
            std::path::PathBuf::from("/run/rem/rem.sock")
        );
    }

    #[test]
    fn daemon_drive_idle_unload_timeout_accepts_zero_and_positive_values() {
        let disabled = with_daemon_lines("drive_idle_unload_seconds = 0\n");
        let config = parse_config_toml(&disabled).expect("zero disables idle unload");
        assert_eq!(config.daemon.drive_idle_unload_seconds, 0);

        let configured = with_daemon_lines("drive_idle_unload_seconds = 45\n");
        let config = parse_config_toml(&configured).expect("positive idle unload timeout");
        assert_eq!(config.daemon.drive_idle_unload_seconds, 45);
    }

    #[test]
    fn daemon_spool_dir_defaults_and_parses_tmpfs_budget_ack() {
        let config = parse_config_toml(&valid_config()).expect("valid config");
        assert_eq!(config.daemon.spool_dir, None);
        assert_eq!(
            config.daemon.spool_dir_or_default(),
            config.daemon.state_dir.join("spool")
        );
        assert_eq!(config.daemon.spool_tmpfs_ram_budget, None);

        let text = valid_config().replace(
            "default_idle_timeout_seconds = 1800",
            "default_idle_timeout_seconds = 1800\nspool_dir = \"/mnt/rem-spool\"\nspool_tmpfs_ram_budget = \"16GiB\"",
        );
        let config = parse_config_toml(&text).expect("valid config with spool_dir");
        assert_eq!(
            config.daemon.spool_dir_or_default(),
            std::path::PathBuf::from("/mnt/rem-spool")
        );
        assert_eq!(
            config.daemon.spool_tmpfs_ram_budget,
            Some(16 * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn daemon_relative_spool_dir_and_zero_tmpfs_budget_are_rejected() {
        let text = valid_config().replace(
            "default_idle_timeout_seconds = 1800",
            "default_idle_timeout_seconds = 1800\nspool_dir = \"relative/spool\"",
        );
        let err = parse_config_toml(&text).expect_err("relative spool_dir must fail");
        assert!(err.to_string().contains("daemon.spool_dir"), "{err}");

        let text = valid_config().replace(
            "default_idle_timeout_seconds = 1800",
            "default_idle_timeout_seconds = 1800\nspool_tmpfs_ram_budget = 0",
        );
        let err = parse_config_toml(&text).expect_err("zero tmpfs budget must fail");
        assert!(err.to_string().contains("spool_tmpfs_ram_budget"), "{err}");
    }

    #[test]
    fn daemon_listen_and_tls_parse_together() {
        let text = with_daemon_lines(
            r#"listen = "0.0.0.0:8443"

[daemon.tls]
cert = "/etc/rem/s.crt"
key = "/etc/rem/s.key"
client_ca = "/etc/rem/ca.crt"
"#,
        );
        let config = parse_config_toml(&text).expect("valid config with listen+tls");
        assert_eq!(config.daemon.listen.as_deref(), Some("0.0.0.0:8443"));
        assert_eq!(
            config.daemon.tls.as_ref().unwrap().client_ca,
            std::path::PathBuf::from("/etc/rem/ca.crt")
        );
    }

    #[test]
    fn daemon_listen_without_tls_is_rejected() {
        let text = with_daemon_lines(
            r#"listen = "0.0.0.0:8443"
"#,
        );
        let err = parse_config_toml(&text).expect_err("listen without tls");
        assert!(err.to_string().contains("must be set together"), "{err}");
    }

    #[test]
    fn daemon_tls_without_listen_is_rejected() {
        let text = with_daemon_lines(
            r#"
[daemon.tls]
cert = "/c"
key = "/k"
client_ca = "/ca"
"#,
        );
        let err = parse_config_toml(&text).expect_err("tls without listen");
        assert!(err.to_string().contains("must be set together"), "{err}");
    }

    #[test]
    fn daemon_unparseable_listen_is_rejected() {
        let text = with_daemon_lines(
            r#"listen = "not-an-addr"

[daemon.tls]
cert = "/c"
key = "/k"
client_ca = "/ca"
"#,
        );
        let err = parse_config_toml(&text).expect_err("bad listen");
        assert!(err.to_string().contains("daemon.listen"), "{err}");
    }

    #[test]
    fn daemon_relative_tls_path_is_rejected() {
        let text = with_daemon_lines(
            r#"listen = "0.0.0.0:8443"

[daemon.tls]
cert = "rel/s.crt"
key = "/k"
client_ca = "/ca"
"#,
        );
        let err = parse_config_toml(&text).expect_err("relative cert");
        assert!(err.to_string().contains("daemon.tls.cert"), "{err}");
    }

    #[test]
    fn rejects_relative_paths() {
        let text = valid_config().replace("state_dir = \"/", "state_dir = \"relative/");

        let err = parse_config_toml(&text).expect_err("relative path must fail");
        assert!(err.to_string().contains("daemon.state_dir"));
    }

    #[test]
    fn parses_minimum_config() {
        let config = parse_config_toml(&valid_config()).expect("valid config");
        assert_eq!(config.libraries[0].serial, "LIB001");
        assert!(config.audit.fsync);
        assert_eq!(config.daemon.spool_dir, None);
        assert_eq!(config.daemon.spool_tmpfs_ram_budget, None);
        assert_eq!(
            config.daemon.io_memory_ceiling,
            DEFAULT_IO_MEMORY_CEILING_BYTES
        );
        assert_eq!(config.daemon.append_staging_mode, AppendStagingMode::Serial);
        assert_eq!(config.daemon.append_ring_bytes, DEFAULT_APPEND_RING_BYTES);
        assert_eq!(config.daemon.append_ring_high_pct, 90);
        assert_eq!(config.daemon.append_ring_low_pct, 25);
        assert_eq!(
            config.daemon.checkpoint_max_bytes,
            DEFAULT_CHECKPOINT_MAX_BYTES
        );
        assert_eq!(
            config.daemon.checkpoint_max_objects,
            DEFAULT_CHECKPOINT_MAX_OBJECTS
        );
        assert_eq!(
            config.daemon.checkpoint_max_age_seconds,
            DEFAULT_CHECKPOINT_MAX_AGE_SECONDS
        );
        assert!(config.tape_pools.is_empty());
        assert!(config.tape_pool_rules.is_empty());
        assert_eq!(
            config.tape_io.staging_ring_buffers,
            remanence_library::DEFAULT_TAPE_IO_STAGING_RING_BUFFERS
        );
        assert_eq!(
            config.tape_io.write_batch_blocks,
            remanence_library::DEFAULT_TAPE_IO_BATCH_BLOCKS
        );
        assert_eq!(
            config.tape_io.read_batch_blocks,
            remanence_library::DEFAULT_TAPE_IO_BATCH_BLOCKS
        );
        assert_eq!(
            config.tape_io.position_check_bytes,
            remanence_library::DEFAULT_TAPE_IO_POSITION_CHECK_BYTES
        );
    }

    #[test]
    fn overlap_append_config_parses_and_rejects_invalid_bounds() {
        let configured = with_daemon_lines(
            "io_memory_ceiling = \"12GiB\"\nappend_staging_mode = \"overlap\"\nappend_ring_bytes = \"4GiB\"\nappend_ring_high_pct = 80\nappend_ring_low_pct = 20\n",
        );
        let config = parse_config_toml(&configured).expect("overlap append config");
        assert_eq!(
            config.daemon.append_staging_mode,
            AppendStagingMode::Overlap
        );
        assert_eq!(config.daemon.append_ring_bytes, 4 * 1024 * 1024 * 1024);
        assert_eq!(config.daemon.append_ring_high_pct, 80);
        assert_eq!(config.daemon.append_ring_low_pct, 20);

        let bad_mode = configured.replace(
            "append_staging_mode = \"overlap\"",
            "append_staging_mode = \"auto\"",
        );
        assert!(
            parse_config_toml(&bad_mode).is_err(),
            "v0.1 must not invent auto policy"
        );

        let bad_watermarks =
            configured.replace("append_ring_high_pct = 80", "append_ring_high_pct = 20");
        let error =
            parse_config_toml(&bad_watermarks).expect_err("equal append watermarks must reject");
        assert!(
            error.to_string().contains("append ring watermarks"),
            "{error}"
        );

        let over_ceiling = configured.replace(
            "append_ring_bytes = \"4GiB\"",
            "append_ring_bytes = \"16GiB\"",
        );
        let config = parse_config_toml(&over_ceiling)
            .expect("reservation, not serial-mode config parsing, enforces shared ceiling");
        assert_eq!(config.daemon.append_ring_bytes, 16 * 1024 * 1024 * 1024);
    }

    #[test]
    fn read_reservoir_and_shared_ceiling_parse_and_reject_degenerate_values() {
        let mut configured = with_daemon_lines("io_memory_ceiling = \"12GiB\"\n");
        configured.push_str(
            "\n[tape_io]\nread_reservoir_bytes = \"4GiB\"\nread_reservoir_high_pct = 80\nread_reservoir_low_pct = 20\nposition_check_bytes_ranged = \"128MiB\"\n",
        );
        let config = parse_config_toml(&configured).expect("reservoir config");
        assert_eq!(config.daemon.io_memory_ceiling, 12 * 1024 * 1024 * 1024);
        assert_eq!(config.tape_io.read_reservoir_bytes, 4 * 1024 * 1024 * 1024);
        assert_eq!(config.tape_io.read_reservoir_high_pct, 80);
        assert_eq!(config.tape_io.read_reservoir_low_pct, 20);
        assert_eq!(
            config.tape_io.position_check_bytes_ranged,
            128 * 1024 * 1024
        );

        let bad_watermarks = configured.replace(
            "read_reservoir_high_pct = 80",
            "read_reservoir_high_pct = 20",
        );
        assert!(parse_config_toml(&bad_watermarks)
            .expect_err("equal watermarks rejected")
            .to_string()
            .contains("watermarks"));

        let over_ceiling = configured.replace(
            "read_reservoir_bytes = \"4GiB\"",
            "read_reservoir_bytes = \"16GiB\"",
        );
        assert!(parse_config_toml(&over_ceiling)
            .expect_err("reservoir above ceiling rejected")
            .to_string()
            .contains("io_memory_ceiling"));
    }

    #[test]
    fn tape_io_config_parses_and_rejects_zero_batches() {
        let mut text = valid_config();
        text.push_str(
            r#"
[tape_io]
staging_ring_buffers = 8
write_batch_blocks = 8
read_batch_blocks = 6
position_check_bytes = "16MiB"
"#,
        );
        let config = parse_config_toml(&text).expect("valid tape_io config");
        assert_eq!(config.tape_io.staging_ring_buffers, 8);
        assert_eq!(config.tape_io.write_batch_blocks, 8);
        assert_eq!(config.tape_io.read_batch_blocks, 6);
        assert_eq!(config.tape_io.position_check_bytes, 16 * 1024 * 1024);

        let mut zero_write = valid_config();
        zero_write.push_str(
            r#"
[tape_io]
write_batch_blocks = 0
"#,
        );
        let err = parse_config_toml(&zero_write).expect_err("zero write batch rejects");
        assert!(err.to_string().contains("write_batch_blocks"), "{err}");

        let mut zero_read = valid_config();
        zero_read.push_str(
            r#"
[tape_io]
read_batch_blocks = 0
"#,
        );
        let err = parse_config_toml(&zero_read).expect_err("zero read batch rejects");
        assert!(err.to_string().contains("read_batch_blocks"), "{err}");

        for invalid_ring in [0, 1, 17] {
            let mut invalid = valid_config();
            invalid.push_str(&format!(
                "\n[tape_io]\nstaging_ring_buffers = {invalid_ring}\n"
            ));
            let err = parse_config_toml(&invalid).expect_err("invalid ring depth rejects");
            assert!(err.to_string().contains("staging_ring_buffers"), "{err}");
        }
    }

    #[test]
    fn tape_io_removed_mode_keys_fail_with_migration_message() {
        for removed in ["pipelined_submission", "legacy_single_block"] {
            let mut text = valid_config();
            text.push_str(&format!("\n[tape_io]\n{removed} = true\n"));
            let err = parse_config_toml(&text).expect_err("removed mode key must reject");
            let message = err.to_string();
            assert!(message.contains(&format!("tape_io.{removed}")), "{message}");
            assert!(message.contains("was removed"), "{message}");
            assert!(message.contains("only tape I/O path"), "{message}");
        }
    }

    #[test]
    fn parses_tape_pool_config() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.copy-a"
display_name = "Camera copy A"
copy_class = "copy-a"
content_class = "camera"
selection_policy = "fill-oldest"
watermark_low = 0.90
watermark_high = 0.95
block_size = "512KiB"
min_object_size = "2GiB"

[[tape_pool_rules]]
prefix = "ACM"
pool_id = "camera.copy-a"
"#,
        );

        let config = parse_config_toml(&text).expect("valid pool config");
        assert_eq!(config.tape_pools.len(), 1);
        assert_eq!(config.tape_pool_rules.len(), 1);
        assert_eq!(config.tape_pools[0].id, "camera.copy-a");
        assert_eq!(
            config.tape_pools[0].display_name.as_deref(),
            Some("Camera copy A")
        );
        assert_eq!(config.tape_pools[0].copy_class.as_deref(), Some("copy-a"));
        assert_eq!(
            config.tape_pools[0].content_class.as_deref(),
            Some("camera")
        );
        assert_eq!(
            config.tape_pools[0].selection_policy,
            PoolSelectionPolicyName::FillOldest
        );
        assert_eq!(config.tape_pools[0].watermark_low, 0.90);
        assert_eq!(config.tape_pools[0].watermark_high, 0.95);
        assert_eq!(config.tape_pools[0].block_size_bytes, 512 * 1024);
        assert_eq!(
            config.tape_pools[0].min_object_size_bytes,
            2 * 1024 * 1024 * 1024
        );
        assert_eq!(config.tape_pool_rules[0].prefix, "ACM");
        assert_eq!(config.tape_pool_rules[0].pool_id, "camera.copy-a");
        assert_eq!(
            derive_tape_pool_from_voltag("acm001l9", &config.tape_pool_rules),
            Some("camera.copy-a")
        );
    }

    #[test]
    fn tape_pool_rules_use_longest_prefix() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.default"

[[tape_pools]]
id = "camera.copy-a"

[[tape_pool_rules]]
prefix = "AC"
pool_id = "camera.default"

[[tape_pool_rules]]
prefix = "ACM"
pool_id = "camera.copy-a"
"#,
        );

        let config = parse_config_toml(&text).expect("valid pool rules");
        assert_eq!(
            derive_tape_pool_from_voltag("ACM001L9", &config.tape_pool_rules),
            Some("camera.copy-a")
        );
        assert_eq!(
            derive_tape_pool_from_voltag("ACX001L9", &config.tape_pool_rules),
            Some("camera.default")
        );
        assert_eq!(
            derive_tape_pool_from_voltag("BCM001L9", &config.tape_pool_rules),
            None
        );
    }

    #[test]
    fn rejects_tape_pool_rule_for_unknown_pool() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pool_rules]]
prefix = "ACM"
pool_id = "camera.copy-a"
"#,
        );

        let err = parse_config_toml(&text).expect_err("unknown rule pool must fail");
        assert!(err.to_string().contains("unknown pool id"));
    }

    #[test]
    fn rejects_ambiguous_equal_length_tape_pool_rule_prefixes() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.copy-a"

[[tape_pools]]
id = "camera.copy-b"

[[tape_pool_rules]]
prefix = "ACM"
pool_id = "camera.copy-a"

[[tape_pool_rules]]
prefix = "acm"
pool_id = "camera.copy-b"
"#,
        );

        let err = parse_config_toml(&text).expect_err("duplicate normalized prefix must fail");
        assert!(err.to_string().contains("ambiguous tape pool rule prefix"));
    }

    #[test]
    fn rejects_invalid_tape_pool_rule_prefix() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.copy-a"

[[tape_pool_rules]]
prefix = "AC M"
pool_id = "camera.copy-a"
"#,
        );

        let err = parse_config_toml(&text).expect_err("invalid prefix must fail");
        assert!(err.to_string().contains("tape pool rule prefix"));
    }

    #[test]
    fn tape_pool_selection_config_defaults() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.copy-a"
"#,
        );

        let config = parse_config_toml(&text).expect("valid pool defaults");
        let pool = &config.tape_pools[0];
        assert_eq!(
            pool.selection_policy,
            PoolSelectionPolicyName::CompleteOrFill
        );
        assert_eq!(pool.watermark_low, default_watermark_low());
        assert_eq!(pool.watermark_high, default_watermark_high());
        assert_eq!(pool.block_size_bytes, DEFAULT_TAPE_BLOCK_SIZE_BYTES);
        assert_eq!(pool.min_object_size_bytes, 0);
    }

    #[test]
    fn rejects_invalid_watermark_ordering() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.copy-a"
watermark_low = 0.97
watermark_high = 0.92
"#,
        );

        let err = parse_config_toml(&text).expect_err("invalid watermarks must fail");
        assert!(err.to_string().contains("watermark_low"));
    }

    #[test]
    fn rejects_unknown_pool_selection_policy() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.copy-a"
selection_policy = "most-free"
"#,
        );

        let err = parse_config_toml(&text).expect_err("unknown policy must fail");
        assert!(err.to_string().contains("unknown selection_policy"));
    }

    #[test]
    fn rejects_invalid_min_object_size_string() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.copy-a"
min_object_size = "2XB"
"#,
        );

        let err = parse_config_toml(&text).expect_err("invalid byte size must fail");
        assert!(err.to_string().contains("unsupported byte-size unit"));
    }

    #[test]
    fn rejects_invalid_tape_pool_block_sizes() {
        for block_size in ["0", "384KiB", "17MiB"] {
            let mut text = valid_config();
            text.push_str(&format!(
                r#"
[[tape_pools]]
id = "camera.copy-a"
block_size = {block_size:?}
"#
            ));

            let err = parse_config_toml(&text).expect_err("invalid block size must fail");
            assert!(err.to_string().contains("256KiB, 512KiB, or 1MiB"), "{err}");
        }
    }

    #[test]
    fn rejects_removed_checkpoint_mode_and_rejects_zero_limits() {
        for removed in ["per_object", "batched"] {
            let err = parse_config_toml(&with_daemon_lines(&format!(
                "checkpoint_mode = {removed:?}\n"
            )))
            .expect_err("removed checkpoint mode must reject");
            assert!(
                err.to_string()
                    .contains("checkpoint_mode was removed 2026-07-21; batched is the only mode"),
                "{err}"
            );
        }
        let config = parse_config_toml(&with_daemon_lines(
            "checkpoint_max_bytes = \"64GiB\"\ncheckpoint_max_objects = 17\ncheckpoint_max_age_seconds = 42\n",
        ))
        .expect("valid checkpoint limits");
        assert_eq!(config.daemon.checkpoint_max_bytes, 64 * 1024 * 1024 * 1024);
        assert_eq!(config.daemon.checkpoint_max_objects, 17);
        assert_eq!(config.daemon.checkpoint_max_age_seconds, 42);

        for line in [
            "checkpoint_max_bytes = \"0\"",
            "checkpoint_max_objects = 0",
            "checkpoint_max_age_seconds = 0",
        ] {
            let err = parse_config_toml(&with_daemon_lines(&format!("{line}\n")))
                .expect_err("zero checkpoint limit must reject");
            assert!(err.to_string().contains("must be non-zero"), "{err}");
        }
    }

    #[test]
    fn trusted_volume_policy_covers_all_local_state_paths() {
        let text = with_daemon_lines("socket_path = \"/var/lib/rem/rem.sock\"\n");
        let config = parse_config_toml(&text).expect("valid config");
        let paths = trusted_volume_paths(&config)
            .into_iter()
            .map(|path| path.to_path_buf())
            .collect::<Vec<_>>();

        assert!(paths.contains(&config.daemon.state_dir));
        assert!(paths.contains(&config.daemon.socket_path.clone().unwrap()));
        assert!(paths.contains(&config.journal.dir));
        assert!(paths.contains(&config.audit.dir));
        assert!(paths.contains(&config.index.sqlite_path));
        assert!(paths.contains(&config.cache.tape_catalog_dir));
    }

    #[test]
    fn validates_watermark_band_invariant_boundary() {
        let mut text = valid_config();
        text.push_str(
            r#"
[[tape_pools]]
id = "camera.copy-a"
watermark_low = 0.80
watermark_high = 0.90
min_object_size = 100
"#,
        );
        let config = parse_config_toml(&text).expect("boundary config parses");
        let pool = &config.tape_pools[0];

        validate_tape_pool_capacity_invariant(pool, 1000)
            .expect("band equal to min_object_size is valid");

        let mut too_large = pool.clone();
        too_large.min_object_size_bytes = 101;
        let err = validate_tape_pool_capacity_invariant(&too_large, 1000)
            .expect_err("narrow band must fail");
        assert!(err.to_string().contains("watermark band 100 bytes"));
    }
}
