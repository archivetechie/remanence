//! Public Layer 4 state handle.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ciborium::value::Value as CborValue;
use remanence_parity::{FileTapeFileJournal, ParityScheme};
use sha2::{Digest, Sha256};
use time::Duration;

use crate::audit::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSink, AuditSubject, FileAuditLog, SourceLayer,
};
use crate::calibration::CalibrationControlStore;
use crate::config::{
    derive_tape_pool_from_voltag, load_config, parse_config_toml, validate_trusted_volume_paths,
    RemConfig,
};
use crate::error::StateError;
use crate::index::{
    AdoptBootstrapIdentityInput, AdoptBootstrapIdentityOutcome, AdoptedTapeState,
    AuditReplayReport, CatalogIndex, CatalogResetPreservedTape, ProvisionTapeInput, RebuildReport,
    RebuildTapeJournalInput, RetireTapeInput, RetireTapeOutcome, TapeIoFenceRecord,
    TapeJournalIndexInput, TapeJournalIndexReport, TapeKindFilter, TapePoolProjectionInput,
    TapeRecord,
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
    calibration: CalibrationControlStore,
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

/// Exact physical provenance recorded for one BOT identity adoption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapAdoptionEvidence {
    /// Inquiry serial of the selected library.
    pub library_serial: String,
    /// Inquiry revision of the selected library.
    pub library_revision: String,
    /// Exact storage element to which the medium was returned.
    pub home_slot: u16,
    /// Exact drive element used throughout the physical recheck.
    pub drive_element: u16,
    /// Inquiry serial of the selected drive.
    pub drive_serial: String,
    /// Exact Bootstrap hardware-compression flag (must be false).
    pub bootstrap_drive_compression: bool,
    /// Verified drive configuration used for the physical read (must be false).
    pub configured_drive_compression: bool,
    /// Typed observed post-Bootstrap layout.
    pub physical_tail: BootstrapAdoptionTailEvidence,
}

/// Valid-BOT tail evidence accepted by the durable adoption boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootstrapAdoptionTailEvidence {
    /// Exactly one filemark then EOD follows Bootstrap.
    ExactBootstrapFilemarkEod,
    /// A data record follows Bootstrap or its first filemark.
    DataAfterBootstrap,
    /// Bootstrap is followed immediately by EOD.
    MissingFilemark,
    /// More than one filemark follows Bootstrap.
    ExtraFilemark,
    /// A read error made the valid Bootstrap tail unknowable.
    Ambiguous,
}

impl BootstrapAdoptionTailEvidence {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ExactBootstrapFilemarkEod => "exact_bootstrap_filemark_eod",
            Self::DataAfterBootstrap => "data_after_bootstrap",
            Self::MissingFilemark => "missing_filemark",
            Self::ExtraFilemark => "extra_filemark",
            Self::Ambiguous => "ambiguous",
        }
    }

    const fn adopted_state(self) -> AdoptedTapeState {
        match self {
            Self::ExactBootstrapFilemarkEod => AdoptedTapeState::Ready,
            Self::DataAfterBootstrap
            | Self::MissingFilemark
            | Self::ExtraFilemark
            | Self::Ambiguous => AdoptedTapeState::RecoveryRequired,
        }
    }
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

/// Result of a catalog reset that explicitly preserved selected tape
/// identities. Ordinary unscoped reset continues to return no report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogResetReport {
    /// Exact identities restored into the otherwise empty catalog.
    pub preserved_tapes: Vec<CatalogResetTapeReport>,
}

/// One exact tape identity restored by a scoped catalog reset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogResetTapeReport {
    /// Physical tape UUID retained from the validated source row.
    pub tape_uuid: [u8; 16],
    /// Exact operator-facing volume tag retained from the source row.
    pub voltag: String,
    /// Pool ownership re-derived from the current operator configuration.
    pub pool_id: Option<String>,
    /// Fail-safe catalog state selected for the identity-only restoration.
    pub state: CatalogResetTapeState,
}

/// Fail-safe state assigned to a tape identity restored by scoped reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogResetTapeState {
    /// Source projection proved the tape was ready, unwritten, and unfinalized.
    Ready,
    /// Source projection carried any write, lifecycle, or finalization evidence.
    RecoveryRequired,
}

/// Read-only, all-kinds catalog admission report for a proposed scoped reset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogResetPreflightReport {
    /// SQLite schema version from which reset admission evidence was read.
    pub source_schema_version: u32,
    /// Stable admission token over config, selectors, and source evidence.
    pub preflight_token: String,
    /// Digest that binds the exact admitted request, config bytes, and paths.
    pub request_digest: String,
    /// Whether an existing durable fence admits this exact request as a resume.
    pub resume_exact: bool,
    /// Resolved config and state paths used for this admission decision.
    pub paths: CatalogResetPreflightPaths,
    /// Exact preserve allowlist validated by this preflight.
    pub preserve_tape_voltags: Vec<String>,
    /// Exact erase allowlist validated by this preflight.
    pub allow_erase_tape_voltags: Vec<String>,
    /// Every tape row, including cleaning and unbound retired identities.
    pub tapes: Vec<CatalogResetPreflightTape>,
}

/// Path binding included in catalog-reset preflight output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogResetPathEvidence {
    /// Configured path before filesystem resolution.
    pub configured_path: PathBuf,
    /// Canonical path after resolving filesystem links.
    pub canonical_path: PathBuf,
    /// Whether the configured path itself is a symbolic link.
    pub configured_path_is_symlink: bool,
}

/// Complete local path binding for a catalog-reset preflight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogResetPreflightPaths {
    /// Operator config file.
    pub config: CatalogResetPathEvidence,
    /// State root.
    pub state_dir: CatalogResetPathEvidence,
    /// SQLite projection.
    pub sqlite: CatalogResetPathEvidence,
    /// Audit directory.
    pub audit: CatalogResetPathEvidence,
    /// Journal directory.
    pub journal: CatalogResetPathEvidence,
    /// Tape cache directory.
    pub tape_cache: CatalogResetPathEvidence,
}

/// Typed all-kinds tape row returned by catalog-reset preflight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogResetPreflightTape {
    /// Exact tape UUID.
    pub tape_uuid: [u8; 16],
    /// Bound volume tag, absent for unbound retired history.
    pub voltag: Option<String>,
    /// Catalog kind (`data` or `cleaning`).
    pub kind: String,
    /// Current source-catalog pool assignment.
    pub pool_id: Option<String>,
    /// Source assignment generation.
    pub assignment_generation: u64,
    /// Current source-catalog lifecycle state.
    pub state: String,
    /// Fixed data block size, when present.
    pub block_size: Option<u64>,
    /// Parity scheme identifier, when present.
    pub scheme_id: Option<String>,
    /// Parity data blocks per stripe, when present.
    pub data_blocks_per_stripe: Option<u32>,
    /// Parity blocks per stripe, when present.
    pub parity_blocks_per_stripe: Option<u32>,
    /// Stripes per parity neighborhood, when present.
    pub stripes_per_neighborhood: Option<u32>,
}

impl CatalogResetTapeState {
    /// Stable operator-facing state name used by CLI output.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::RecoveryRequired => "recovery_required",
        }
    }
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

    /// Reset rebuildable catalog state while retaining only explicitly selected
    /// tape identities.
    ///
    /// Every selector and source row is validated under the exclusive state
    /// lock before any archive or deletion. Restored rows contain only UUID,
    /// exact voltag, kind, data geometry, and a pool re-derived from current
    /// config. No physical-prefix, object, tape-file, audit, journal, operation,
    /// session, or idempotency authority is retained.
    pub fn reset_catalog_preserving_from_config_file(
        config_path: impl AsRef<Path>,
        preserve_tape_voltags: &[String],
    ) -> Result<CatalogResetReport, StateError> {
        Self::reset_catalog_preserving_with_allowlist_from_config_file(
            config_path,
            preserve_tape_voltags,
            &[],
        )
    }

    /// Scoped reset with an exact caller-supplied allowlist for bound tapes
    /// that may be erased instead of preserved.
    pub fn reset_catalog_preserving_with_allowlist_from_config_file(
        config_path: impl AsRef<Path>,
        preserve_tape_voltags: &[String],
        allow_erase_tape_voltags: &[String],
    ) -> Result<CatalogResetReport, StateError> {
        let preserve = validate_catalog_reset_selectors(preserve_tape_voltags, "preserve")?;
        let allow_erase =
            validate_catalog_reset_selectors(allow_erase_tape_voltags, "allow-erase")?;
        validate_disjoint_catalog_reset_allowlists(&preserve, &allow_erase)?;
        let config_path = config_path.as_ref();
        let config = load_config(config_path)?;
        let paths = StatePaths::from_config(config_path, &config);
        Self::reset_catalog_with_config_preserving_allowlist(paths, config, &preserve, &allow_erase)
    }

    /// Scoped reset admitted only when source evidence still matches a prior
    /// preflight token.
    pub fn reset_catalog_preserving_with_preflight_token_from_config_file(
        config_path: impl AsRef<Path>,
        preserve_tape_voltags: &[String],
        allow_erase_tape_voltags: &[String],
        expected_preflight_token: &str,
    ) -> Result<CatalogResetReport, StateError> {
        if expected_preflight_token.len() != 64
            || !expected_preflight_token
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(StateError::ConfigInvalid(
                "expected preflight token must be 64 lowercase hexadecimal characters".to_string(),
            ));
        }
        let preserve = validate_catalog_reset_selectors(preserve_tape_voltags, "preserve")?;
        let allow_erase =
            validate_catalog_reset_selectors(allow_erase_tape_voltags, "allow-erase")?;
        validate_disjoint_catalog_reset_allowlists(&preserve, &allow_erase)?;
        let config_path = config_path.as_ref();
        let config = load_config(config_path)?;
        let paths = StatePaths::from_config(config_path, &config);
        Self::reset_catalog_with_config_preserving_allowlist_expected(
            paths,
            config,
            &preserve,
            &allow_erase,
            Some(expected_preflight_token),
        )
    }

    /// Perform a read-only, all-kinds admission check for a proposed scoped
    /// reset. This is advisory; the mutating API repeats the same check while
    /// holding its exclusive lock.
    pub fn preflight_catalog_reset_from_config_file(
        config_path: impl AsRef<Path>,
        preserve_tape_voltags: &[String],
        allow_erase_tape_voltags: &[String],
    ) -> Result<CatalogResetPreflightReport, StateError> {
        let preserve = validate_catalog_reset_selectors(preserve_tape_voltags, "preserve")?;
        let allow_erase =
            validate_catalog_reset_selectors(allow_erase_tape_voltags, "allow-erase")?;
        validate_disjoint_catalog_reset_allowlists(&preserve, &allow_erase)?;
        let config_path = config_path.as_ref();
        let config = load_config(config_path)?;
        let paths = StatePaths::from_config(config_path, &config);
        let _lock = StateLockGuard::acquire(&paths.state_dir)?;
        let (locked_config, config_bytes) = load_catalog_reset_config_snapshot(config_path)?;
        ensure_catalog_reset_config_unchanged(&paths, &config, &locked_config)?;
        let (mut report, _, output_is_clean) = preflight_catalog_reset_source(
            &paths,
            &locked_config,
            &config_bytes,
            &preserve,
            &allow_erase,
        )?;
        if let Some((stored_request, stored_token, stored_output)) =
            catalog_reset_fence_evidence(&paths)?
        {
            let expected_request = catalog_reset_request_digest(
                &paths,
                &config_bytes,
                &preserve,
                &allow_erase,
                false,
                &stored_token,
            )?;
            if stored_request != expected_request {
                return Err(StateError::CatalogResetInProgress(
                    "durable fence does not match this preflight request".to_string(),
                ));
            }
            if report.preflight_token != stored_token
                && (!output_is_clean
                    || catalog_reset_current_output_token(&report.tapes) != stored_output)
            {
                return Err(StateError::CatalogResetInProgress(
                    "catalog source changed during an interrupted reset".to_string(),
                ));
            }
            report.preflight_token = stored_token;
            report.request_digest = stored_request;
            report.resume_exact = true;
        } else {
            report.request_digest = catalog_reset_request_digest(
                &paths,
                &config_bytes,
                &preserve,
                &allow_erase,
                false,
                &report.preflight_token,
            )?;
        }
        Ok(report)
    }

    /// Open state with already-resolved paths and a parsed config.
    pub fn open_with_config(paths: StatePaths, config: RemConfig) -> Result<Self, StateError> {
        let lock = StateLockGuard::acquire(&paths.state_dir)?;
        ensure_no_catalog_reset_fence(&paths)?;
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
        let calibration = CalibrationControlStore::open(&paths.calibration_dir)?;
        Ok(Self {
            paths,
            config,
            config_warnings,
            _lock: lock,
            audit,
            index,
            calibration,
        })
    }

    /// Reset local catalog state with already-resolved paths and parsed config.
    pub fn reset_catalog_with_config(
        paths: StatePaths,
        config: RemConfig,
    ) -> Result<(), StateError> {
        let _lock = StateLockGuard::acquire(&paths.state_dir)?;
        let (locked_config, config_bytes) = load_catalog_reset_config_snapshot(&paths.config_path)?;
        ensure_catalog_reset_config_unchanged(&paths, &config, &locked_config)?;
        let token = "0".repeat(64);
        let request_digest =
            catalog_reset_request_digest(&paths, &config_bytes, &[], &[], true, &token)?;
        reset_catalog_locked(
            &paths,
            &locked_config,
            &[],
            CatalogResetAdmission {
                request_digest: &request_digest,
                preflight_token: &token,
                output_token: &token,
                source_schema_version: None,
            },
        )
    }

    /// Scoped counterpart of [`Self::reset_catalog_with_config`] for callers
    /// that already parsed config and resolved state paths.
    pub fn reset_catalog_with_config_preserving(
        paths: StatePaths,
        config: RemConfig,
        preserve_tape_voltags: &[String],
    ) -> Result<CatalogResetReport, StateError> {
        Self::reset_catalog_with_config_preserving_allowlist(
            paths,
            config,
            preserve_tape_voltags,
            &[],
        )
    }

    /// Scoped reset with exact preserve and erase allowlists for callers that
    /// already parsed config and resolved state paths.
    pub fn reset_catalog_with_config_preserving_allowlist(
        paths: StatePaths,
        config: RemConfig,
        preserve_tape_voltags: &[String],
        allow_erase_tape_voltags: &[String],
    ) -> Result<CatalogResetReport, StateError> {
        Self::reset_catalog_with_config_preserving_allowlist_expected(
            paths,
            config,
            preserve_tape_voltags,
            allow_erase_tape_voltags,
            None,
        )
    }

    fn reset_catalog_with_config_preserving_allowlist_expected(
        paths: StatePaths,
        config: RemConfig,
        preserve_tape_voltags: &[String],
        allow_erase_tape_voltags: &[String],
        expected_preflight_token: Option<&str>,
    ) -> Result<CatalogResetReport, StateError> {
        let preserve = validate_catalog_reset_selectors(preserve_tape_voltags, "preserve")?;
        let allow_erase =
            validate_catalog_reset_selectors(allow_erase_tape_voltags, "allow-erase")?;
        validate_disjoint_catalog_reset_allowlists(&preserve, &allow_erase)?;
        let _lock = StateLockGuard::acquire(&paths.state_dir)?;
        let (locked_config, config_bytes) = load_catalog_reset_config_snapshot(&paths.config_path)?;
        ensure_catalog_reset_config_unchanged(&paths, &config, &locked_config)?;
        let (preflight, preserved_tapes, output_is_clean) = preflight_catalog_reset_source(
            &paths,
            &locked_config,
            &config_bytes,
            &preserve,
            &allow_erase,
        )?;
        let mut already_swapped = false;
        let (token, output_token) = if let Some((stored_request, stored_token, stored_output)) =
            catalog_reset_fence_evidence(&paths)?
        {
            if expected_preflight_token.is_some_and(|expected| expected != stored_token) {
                return Err(StateError::CatalogResetInProgress(
                    "expected preflight token does not match the durable reset fence".to_string(),
                ));
            }
            let request = catalog_reset_request_digest(
                &paths,
                &config_bytes,
                &preserve,
                &allow_erase,
                false,
                &stored_token,
            )?;
            if request != stored_request {
                return Err(StateError::CatalogResetInProgress(
                    "durable fence does not match this reset request".to_string(),
                ));
            }
            if preflight.preflight_token != stored_token
                && (!output_is_clean
                    || catalog_reset_current_output_token(&preflight.tapes) != stored_output)
            {
                return Err(StateError::CatalogResetInProgress(
                    "catalog source changed during an interrupted reset".to_string(),
                ));
            }
            already_swapped = preflight.preflight_token != stored_token;
            (stored_token, stored_output)
        } else {
            if expected_preflight_token
                .is_some_and(|expected| expected != preflight.preflight_token)
            {
                return Err(StateError::ConfigInvalid(
                    "catalog reset source changed after preflight; run preflight again".to_string(),
                ));
            }
            let intended_output =
                catalog_reset_intended_output_token(&preflight.tapes, &preserved_tapes)?;
            (preflight.preflight_token.clone(), intended_output)
        };
        let request_digest = catalog_reset_request_digest(
            &paths,
            &config_bytes,
            &preserve,
            &allow_erase,
            false,
            &token,
        )?;
        if already_swapped {
            sync_regular_file(&paths.sqlite_path)?;
            if let Some(parent) = paths.sqlite_path.parent() {
                sync_directory(parent)?;
            }
            sync_directory(&paths.audit_dir)?;
            sync_directory(&paths.journal_dir)?;
            sync_directory(&paths.tape_cache_dir)?;
            clear_catalog_reset_fence(&paths)?;
            let preserve_set = preserve.iter().map(String::as_str).collect::<HashSet<_>>();
            return Ok(CatalogResetReport {
                preserved_tapes: preflight
                    .tapes
                    .into_iter()
                    .filter_map(|tape| {
                        let voltag = tape.voltag?;
                        preserve_set
                            .contains(voltag.as_str())
                            .then(|| CatalogResetTapeReport {
                                tape_uuid: tape.tape_uuid,
                                voltag,
                                pool_id: tape.pool_id,
                                state: if tape.state == "ready" {
                                    CatalogResetTapeState::Ready
                                } else {
                                    CatalogResetTapeState::RecoveryRequired
                                },
                            })
                    })
                    .collect(),
            });
        }
        reset_catalog_locked(
            &paths,
            &locked_config,
            &preserved_tapes,
            CatalogResetAdmission {
                request_digest: &request_digest,
                preflight_token: &token,
                output_token: &output_token,
                source_schema_version: Some(preflight.source_schema_version),
            },
        )?;
        Ok(CatalogResetReport {
            preserved_tapes: preserved_tapes
                .into_iter()
                .map(|tape| CatalogResetTapeReport {
                    tape_uuid: tape.tape_uuid,
                    voltag: tape.voltag,
                    pool_id: tape.pool_id,
                    state: if tape.restore_ready {
                        CatalogResetTapeState::Ready
                    } else {
                        CatalogResetTapeState::RecoveryRequired
                    },
                })
                .collect(),
        })
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

    /// Provision or reprovision one tape while coordinating any old physical
    /// identity's durable wrap-map calibration eviction.
    pub fn provision_tape(&mut self, input: ProvisionTapeInput) -> Result<(), StateError> {
        let prior = match self.index.get_tape(&input.tape_uuid)? {
            Some(tape) => Some(tape),
            None => self.index.get_tape_by_voltag(input.voltag.as_str())?,
        };
        let prior_uuid_to_evict = prior
            .as_ref()
            .filter(|tape| {
                tape.tape_uuid.as_slice() != input.tape_uuid
                    || !tape_record_matches_provision_geometry(tape, &input)
            })
            .map(|tape| {
                <[u8; 16]>::try_from(tape.tape_uuid.as_slice()).map_err(|_| {
                    StateError::IndexCorrupt(format!(
                        "provisioning matched tape row with {}-byte uuid",
                        tape.tape_uuid.len()
                    ))
                })
            })
            .transpose()?;
        self.index.provision_tape(input)?;
        if let Some(prior_uuid) = prior_uuid_to_evict {
            self.calibration.record_map_evicted(prior_uuid)?;
        }
        Ok(())
    }

    /// Durably adopt a checksum-valid BOT Bootstrap as identity-only state.
    ///
    /// Existing adoption audit is replayed before preview. An exact
    /// identity-only projection may be returned as a no-op even when a later
    /// physical recheck uses a fresh operation UUID. Reusing one operation UUID
    /// for changed facts still conflicts. For a new identity, the audit record
    /// is fsynced before the SQLite projection; a crash in between is completed
    /// by the same replay-before-preview sequence on the next invocation. No
    /// tape-file or Object authority is created.
    pub fn adopt_bootstrap_identity(
        &mut self,
        input: AdoptBootstrapIdentityInput,
        evidence: BootstrapAdoptionEvidence,
    ) -> Result<AdoptBootstrapIdentityOutcome, StateError> {
        if input.operation_id.is_nil() {
            return Err(StateError::ConfigInvalid(
                "bootstrap adoption operation_id must not be nil".to_string(),
            ));
        }
        if input.tape_uuid[6] >> 4 != 4 || input.tape_uuid[8] & 0xc0 != 0x80 {
            return Err(StateError::TapeProvisionConflict(
                "Bootstrap tape UUID must be an RFC 4122 UUIDv4".to_string(),
            ));
        }
        let canonical_parity = remanence_parity::ParityConfig::Scheme(
            remanence_parity::default_scheme_for_block_size(1024 * 1024),
        );
        if input.block_size != 1024 * 1024
            || input.parity != canonical_parity
            || evidence.bootstrap_drive_compression
            || evidence.configured_drive_compression
        {
            return Err(StateError::TapeProvisionConflict(
                "bootstrap adoption requires canonical 1 MiB default parity geometry with drive compression disabled"
                    .to_string(),
            ));
        }
        for (name, value) in [
            ("library_serial", evidence.library_serial.as_str()),
            ("library_revision", evidence.library_revision.as_str()),
            ("drive_serial", evidence.drive_serial.as_str()),
        ] {
            if value.trim().is_empty() || value.trim() != value {
                return Err(StateError::ConfigInvalid(format!(
                    "bootstrap adoption {name} must be non-empty and trimmed"
                )));
            }
        }
        let state = evidence.physical_tail.adopted_state();
        let pool_id =
            derive_tape_pool_from_voltag(input.voltag.as_str(), &self.config.tape_pool_rules)
                .ok_or_else(|| {
                    StateError::TapeProvisionConflict(format!(
                        "bootstrap barcode {:?} does not match any locked tape_pool_rule",
                        input.voltag
                    ))
                })?
                .to_string();
        if !self.config.tape_pools.iter().any(|pool| pool.id == pool_id) {
            return Err(StateError::TapeProvisionConflict(format!(
                "bootstrap barcode {:?} derives unconfigured pool {pool_id:?}",
                input.voltag
            )));
        }
        let geometry = match &input.parity {
            remanence_parity::ParityConfig::None => "no-parity".to_string(),
            remanence_parity::ParityConfig::Scheme(scheme) => format!(
                "scheme={} data={} parity={} stripes={}",
                scheme.id.as_str(),
                scheme.data_blocks_per_stripe,
                scheme.parity_blocks_per_stripe,
                scheme.stripes_per_neighborhood
            ),
        };
        let request_fingerprint = bootstrap_adoption_request_fingerprint(
            &input,
            &evidence,
            pool_id.as_str(),
            state,
            geometry.as_str(),
        );
        let records = FileAuditLog::replay(&self.paths.audit_dir)?;
        self.index.replay_audit_records(&records)?;
        let tape_subject = hex_tape_uuid(input.tape_uuid);
        let mut matching_operation_authority = false;
        let mut tape_adoption_authority = false;
        let mut tape_adoption_generation = None;
        for record in &records {
            let operation_match = record.operation_id == Some(input.operation_id);
            let tape_match = record.event == AuditEvent::TapeIdentityAdopted
                && record.subject.kind == "tape"
                && record.subject.id.as_deref() == Some(tape_subject.as_str());
            if tape_match {
                tape_adoption_authority = true;
                let recorded_generation = record
                    .detail
                    .get("assignment_generation")
                    .and_then(|value| match value {
                        CborValue::Integer(value) => u64::try_from(i128::from(*value)).ok(),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        StateError::IndexCorrupt(format!(
                            "TapeIdentityAdopted record {} has no valid assignment generation",
                            record.record_uuid
                        ))
                    })?;
                if tape_adoption_generation
                    .replace(recorded_generation)
                    .is_some_and(|prior| prior != recorded_generation)
                {
                    return Err(StateError::IndexCorrupt(format!(
                        "tape {tape_subject} has conflicting adoption assignment generations"
                    )));
                }
            }
            if !operation_match {
                continue;
            }
            if record.event != AuditEvent::TapeIdentityAdopted || !tape_match {
                return Err(StateError::TapeProvisionConflict(format!(
                    "operation {} is already bound to different {:?} authority",
                    input.operation_id, record.event
                )));
            }
            let recorded_fingerprint = record
                .detail
                .get("request_fingerprint")
                .and_then(|value| match value {
                    CborValue::Bytes(bytes) => <[u8; 32]>::try_from(bytes.as_slice()).ok(),
                    _ => None,
                })
                .ok_or_else(|| {
                    StateError::IndexCorrupt(format!(
                        "TapeIdentityAdopted record {} has no valid request fingerprint",
                        record.record_uuid
                    ))
                })?;
            if recorded_fingerprint != request_fingerprint {
                return Err(StateError::TapeProvisionConflict(format!(
                    "bootstrap adoption operation {} was reused with changed immutable facts",
                    input.operation_id
                )));
            }
            matching_operation_authority = true;
        }
        let preview = self.index.preview_bootstrap_identity_adoption(
            &input,
            pool_id.as_str(),
            state,
            request_fingerprint,
        )?;
        if matching_operation_authority || tape_adoption_authority {
            let outcome = preview.ok_or_else(|| {
                StateError::IndexCorrupt(format!(
                    "adoption audit for tape {tape_subject} did not project its identity"
                ))
            })?;
            if tape_adoption_generation != Some(outcome.assignment_generation) {
                return Err(StateError::TapeProvisionConflict(format!(
                    "Bootstrap identity {tape_subject} assignment generation evolved after adoption"
                )));
            }
            return Ok(outcome);
        }
        let projected_assignment_generation = preview
            .as_ref()
            .map(|outcome| outcome.assignment_generation)
            .unwrap_or(1);
        let mut detail = BTreeMap::from([
            ("voltag".to_string(), CborValue::Text(input.voltag.clone())),
            ("pool_id".to_string(), CborValue::Text(pool_id.clone())),
            (
                "block_size".to_string(),
                CborValue::Integer(input.block_size.into()),
            ),
            ("geometry".to_string(), CborValue::Text(geometry)),
            (
                "request_fingerprint".to_string(),
                CborValue::Bytes(request_fingerprint.to_vec()),
            ),
            (
                "state".to_string(),
                CborValue::Text(state.as_str().to_string()),
            ),
            (
                "assignment_generation".to_string(),
                CborValue::Integer(projected_assignment_generation.into()),
            ),
            (
                "library_serial".to_string(),
                CborValue::Text(evidence.library_serial),
            ),
            (
                "library_revision".to_string(),
                CborValue::Text(evidence.library_revision),
            ),
            (
                "home_slot".to_string(),
                CborValue::Integer(evidence.home_slot.into()),
            ),
            (
                "drive_element".to_string(),
                CborValue::Integer(evidence.drive_element.into()),
            ),
            (
                "drive_serial".to_string(),
                CborValue::Text(evidence.drive_serial),
            ),
            (
                "bootstrap_drive_compression".to_string(),
                CborValue::Bool(evidence.bootstrap_drive_compression),
            ),
            (
                "configured_drive_compression".to_string(),
                CborValue::Bool(evidence.configured_drive_compression),
            ),
            (
                "physical_tail".to_string(),
                CborValue::Text(evidence.physical_tail.as_str().to_string()),
            ),
        ]);
        detail.insert("identity_only".to_string(), CborValue::Bool(true));
        self.audit.append(AuditEventRecord {
            actor: AuditActor::local_user(),
            source_layer: SourceLayer::Layer4,
            operation_id: Some(input.operation_id),
            session_id: None,
            idempotency_key: Some(input.operation_id),
            event: AuditEvent::TapeIdentityAdopted,
            subject: AuditSubject {
                kind: "tape".to_string(),
                id: Some(tape_subject),
            },
            detail,
        })?;

        self.index
            .adopt_bootstrap_identity(input, pool_id.as_str(), state, request_fingerprint)
    }

    /// Return the durable calibration-control store (cloneable
    /// handle). This is the authority on wrap-map servability; it
    /// survives projection rebuild and catalog reset.
    pub fn calibration_control(&self) -> &CalibrationControlStore {
        &self.calibration
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
        let report = self
            .index
            .rebuild_from_authoritative_sources(&audit_records, &tape_journals)?;
        // Rebuild cleared the wrap_maps projection (design §6.5:
        // "catalog projection rebuild → UNCALIBRATED for every
        // evicted map; control rows and allocator remain"). Record
        // the matching durable transitions so every volume carries a
        // fresh generation and stays uncalibrated until its next
        // load harvest.
        self.calibration
            .record_all_maps_evicted("projection_rebuild")?;
        Ok(report)
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

    /// Release a tape-I/O fence and append its durable release evidence.
    pub fn release_tape_io_fence(
        &mut self,
        quarantine_id: &str,
        ack: &str,
    ) -> Result<Option<TapeIoFenceRecord>, StateError> {
        let released = self.index.release_tape_io_fence(quarantine_id, ack)?;
        let Some(record) = released else {
            return Ok(None);
        };
        let mut detail = BTreeMap::from([
            (
                "tape_uuid".to_string(),
                CborValue::Bytes(record.tape_uuid.clone()),
            ),
            (
                "quarantine_id".to_string(),
                CborValue::Text(record.quarantine_id.clone()),
            ),
            ("release_ack".to_string(), CborValue::Text(ack.to_string())),
        ]);
        if let Some(barcode) = record.barcode.as_ref() {
            detail.insert("barcode".to_string(), CborValue::Text(barcode.clone()));
        }
        self.audit.append(AuditEventRecord {
            actor: AuditActor::local_user(),
            source_layer: SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: AuditEvent::TapeIoFenceReleased,
            subject: AuditSubject {
                kind: "tape_io_fence".to_string(),
                id: Some(record.quarantine_id.clone()),
            },
            detail,
        })?;
        Ok(Some(record))
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
        if !state.orphaned_bundles.is_empty() {
            tracing::warn!(
                tape_uuid = %uuid::Uuid::from_bytes(tape_uuid),
                orphaned_bundle_count = state.orphaned_bundles.len(),
                "ignored sink-journal bundles beyond the last checkpoint watermark"
            );
        }
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
            if !state.orphaned_bundles.is_empty() {
                tracing::warn!(
                    tape_uuid = %uuid::Uuid::from_bytes(input.tape_uuid),
                    orphaned_bundle_count = state.orphaned_bundles.len(),
                    "ignored sink-journal bundles beyond the last checkpoint watermark during rebuild"
                );
            }
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
            // A write session that was non-terminal when the process
            // died may have dispatched a media-modifying CDB whose
            // fence state is now uncertain. The design's §6.5 startup
            // row resolves that uncertainty as false invalidation:
            // durably advance the epoch and leave the volume
            // uncalibrated until a fresh load harvest. Read sessions
            // dispatch nothing media-modifying and are left alone.
            if session.session_kind == "write" {
                if let Some(tape_uuid) = session
                    .tape_uuid
                    .as_deref()
                    .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
                {
                    self.calibration.record_possible_write_recovery(tape_uuid)?;
                }
            }
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

fn tape_record_matches_provision_geometry(tape: &TapeRecord, input: &ProvisionTapeInput) -> bool {
    if tape.block_size != Some(u64::from(input.block_size)) {
        return false;
    }
    match &input.parity {
        remanence_parity::ParityConfig::None => {
            tape.scheme_id.is_none()
                && tape.data_blocks_per_stripe.is_none()
                && tape.parity_blocks_per_stripe.is_none()
                && tape.stripes_per_neighborhood.is_none()
        }
        remanence_parity::ParityConfig::Scheme(scheme) => {
            tape.scheme_id.as_deref() == Some(scheme.id.as_str())
                && tape.data_blocks_per_stripe == Some(u32::from(scheme.data_blocks_per_stripe))
                && tape.parity_blocks_per_stripe == Some(u32::from(scheme.parity_blocks_per_stripe))
                && tape.stripes_per_neighborhood == Some(scheme.stripes_per_neighborhood)
        }
    }
}

fn bootstrap_adoption_request_fingerprint(
    input: &AdoptBootstrapIdentityInput,
    evidence: &BootstrapAdoptionEvidence,
    pool_id: &str,
    state: AdoptedTapeState,
    geometry: &str,
) -> [u8; 32] {
    fn field(hash: &mut Sha256, name: &str, value: &[u8]) {
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value);
    }

    let mut hash = Sha256::new();
    hash.update(b"remanence.bootstrap-adoption-request.v1\0");
    field(&mut hash, "tape_uuid", input.tape_uuid.as_slice());
    field(&mut hash, "voltag", input.voltag.as_bytes());
    field(&mut hash, "block_size", &input.block_size.to_be_bytes());
    field(&mut hash, "geometry", geometry.as_bytes());
    field(&mut hash, "pool_id", pool_id.as_bytes());
    field(&mut hash, "state", state.as_str().as_bytes());
    field(
        &mut hash,
        "library_serial",
        evidence.library_serial.as_bytes(),
    );
    field(
        &mut hash,
        "library_revision",
        evidence.library_revision.as_bytes(),
    );
    field(&mut hash, "home_slot", &evidence.home_slot.to_be_bytes());
    field(
        &mut hash,
        "drive_element",
        &evidence.drive_element.to_be_bytes(),
    );
    field(&mut hash, "drive_serial", evidence.drive_serial.as_bytes());
    field(
        &mut hash,
        "bootstrap_drive_compression",
        &[u8::from(evidence.bootstrap_drive_compression)],
    );
    field(
        &mut hash,
        "configured_drive_compression",
        &[u8::from(evidence.configured_drive_compression)],
    );
    field(
        &mut hash,
        "physical_tail",
        evidence.physical_tail.as_str().as_bytes(),
    );
    hash.finalize().into()
}

fn validate_catalog_reset_selectors(
    selectors: &[String],
    role: &str,
) -> Result<Vec<String>, StateError> {
    let mut seen = HashSet::with_capacity(selectors.len());
    let mut validated = Vec::with_capacity(selectors.len());
    for selector in selectors {
        if selector.is_empty() || selector.trim().is_empty() {
            return Err(StateError::ConfigInvalid(format!(
                "catalog reset {role} selector must be nonblank"
            )));
        }
        if selector.trim() != selector {
            return Err(StateError::ConfigInvalid(format!(
                "catalog reset {role} selector {selector:?} must not contain surrounding whitespace"
            )));
        }
        if !seen.insert(selector.clone()) {
            return Err(StateError::ConfigInvalid(format!(
                "duplicate catalog reset {role} selector {selector:?}"
            )));
        }
        validated.push(selector.clone());
    }
    Ok(validated)
}

fn validate_disjoint_catalog_reset_allowlists(
    preserve: &[String],
    allow_erase: &[String],
) -> Result<(), StateError> {
    let preserve = preserve.iter().collect::<HashSet<_>>();
    if let Some(overlap) = allow_erase.iter().find(|voltag| preserve.contains(voltag)) {
        return Err(StateError::ConfigInvalid(format!(
            "catalog reset voltag {overlap:?} appears in both preserve and allow-erase lists"
        )));
    }
    Ok(())
}

fn preflight_catalog_reset_index(
    index: &CatalogIndex,
    paths: &StatePaths,
    config: &RemConfig,
    config_bytes: &[u8],
    preserve: &[String],
    allow_erase: &[String],
) -> Result<
    (
        CatalogResetPreflightReport,
        Vec<CatalogResetPreservedTape>,
        bool,
    ),
    StateError,
> {
    let preserved_tapes = index.capture_catalog_reset_tapes(preserve, &config.tape_pool_rules)?;
    let source_schema_version = index.schema_version()?;
    let source_tapes = index.list_tapes(None, TapeKindFilter::All)?;
    let output_is_clean = index.catalog_reset_output_is_clean()?
        && catalog_reset_output_pools_match(index, config)?
        && catalog_reset_output_directories_clean(paths)?;
    preflight_catalog_reset_rows(
        paths,
        config_bytes,
        preserve,
        allow_erase,
        source_schema_version,
        source_tapes,
        preserved_tapes,
        output_is_clean,
    )
}

fn preflight_catalog_reset_source(
    paths: &StatePaths,
    config: &RemConfig,
    config_bytes: &[u8],
    preserve: &[String],
    allow_erase: &[String],
) -> Result<
    (
        CatalogResetPreflightReport,
        Vec<CatalogResetPreservedTape>,
        bool,
    ),
    StateError,
> {
    match CatalogIndex::open_read_only(&paths.sqlite_path) {
        Ok(index) => preflight_catalog_reset_index(
            &index,
            paths,
            config,
            config_bytes,
            preserve,
            allow_erase,
        ),
        Err(current_schema_error) => {
            if !preserve.is_empty() || allow_erase.is_empty() {
                return Err(current_schema_error);
            }
            let (source_schema_version, source_tapes) =
                CatalogIndex::read_legacy_catalog_reset_erase_source(&paths.sqlite_path)?;
            preflight_catalog_reset_rows(
                paths,
                config_bytes,
                preserve,
                allow_erase,
                source_schema_version,
                source_tapes,
                Vec::new(),
                false,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn preflight_catalog_reset_rows(
    paths: &StatePaths,
    config_bytes: &[u8],
    preserve: &[String],
    allow_erase: &[String],
    source_schema_version: u32,
    source_tapes: Vec<TapeRecord>,
    preserved_tapes: Vec<CatalogResetPreservedTape>,
    output_is_clean: bool,
) -> Result<
    (
        CatalogResetPreflightReport,
        Vec<CatalogResetPreservedTape>,
        bool,
    ),
    StateError,
> {
    let preserve_set = preserve.iter().map(String::as_str).collect::<HashSet<_>>();
    let erase_set = allow_erase
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut seen_uuids = HashSet::with_capacity(source_tapes.len());
    let mut seen_voltags = HashSet::with_capacity(source_tapes.len());
    let mut tapes = Vec::with_capacity(source_tapes.len());
    for tape in source_tapes {
        let typed = catalog_reset_preflight_tape(tape)?;
        if !seen_uuids.insert(typed.tape_uuid) {
            return Err(StateError::AmbiguousCatalogLookup(format!(
                "catalog reset preflight found duplicate tape uuid {}",
                hex_tape_uuid(typed.tape_uuid)
            )));
        }
        match typed.voltag.as_deref() {
            Some(voltag) => {
                if !seen_voltags.insert(voltag.to_string()) {
                    return Err(StateError::AmbiguousCatalogLookup(format!(
                        "catalog reset preflight found duplicate voltag {voltag:?}"
                    )));
                }
                if !preserve_set.contains(voltag) && !erase_set.contains(voltag) {
                    return Err(StateError::ConfigInvalid(format!(
                        "catalog reset refuses bound tape {voltag:?} outside the exact preserve and allow-erase lists"
                    )));
                }
            }
            None if typed.state == "retired" => {}
            None => {
                return Err(StateError::ConfigInvalid(format!(
                    "catalog reset refuses unbound nonretired tape {} in state {:?}",
                    hex_tape_uuid(typed.tape_uuid),
                    typed.state
                )));
            }
        }
        tapes.push(typed);
    }
    let preflight_token = catalog_reset_preflight_token(
        paths,
        config_bytes,
        preserve,
        allow_erase,
        source_schema_version,
        &tapes,
        &preserved_tapes,
    )?;
    Ok((
        CatalogResetPreflightReport {
            source_schema_version,
            preflight_token,
            request_digest: String::new(),
            resume_exact: false,
            paths: catalog_reset_preflight_paths(paths)?,
            preserve_tape_voltags: preserve.to_vec(),
            allow_erase_tape_voltags: allow_erase.to_vec(),
            tapes,
        },
        preserved_tapes,
        output_is_clean,
    ))
}

fn catalog_reset_output_pools_match(
    index: &CatalogIndex,
    config: &RemConfig,
) -> Result<bool, StateError> {
    let mut actual = index
        .list_tape_pools()?
        .into_iter()
        .map(|pool| {
            (
                pool.pool_id,
                pool.display_name,
                pool.copy_class,
                pool.content_class,
            )
        })
        .collect::<Vec<_>>();
    let mut expected = config
        .tape_pools
        .iter()
        .map(|pool| {
            (
                pool.id.clone(),
                pool.display_name.clone(),
                pool.copy_class.clone(),
                pool.content_class.clone(),
            )
        })
        .collect::<Vec<_>>();
    actual.sort();
    expected.sort();
    Ok(actual == expected)
}

fn catalog_reset_output_directories_clean(paths: &StatePaths) -> Result<bool, StateError> {
    for path in [&paths.audit_dir, &paths.journal_dir, &paths.tape_cache_dir] {
        let metadata = fs::symlink_metadata(path).map_err(|err| {
            StateError::io_at("inspect catalog reset output directory", path, err)
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Ok(false);
        }
        let mut entries = fs::read_dir(path)
            .map_err(|err| StateError::io_at("read catalog reset output directory", path, err))?;
        if entries
            .next()
            .transpose()
            .map_err(|err| StateError::io_at("iterate catalog reset output directory", path, err))?
            .is_some()
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn catalog_reset_preflight_paths(
    paths: &StatePaths,
) -> Result<CatalogResetPreflightPaths, StateError> {
    Ok(CatalogResetPreflightPaths {
        config: catalog_reset_path_evidence(&paths.config_path)?,
        state_dir: catalog_reset_path_evidence(&paths.state_dir)?,
        sqlite: catalog_reset_path_evidence(&paths.sqlite_path)?,
        audit: catalog_reset_path_evidence(&paths.audit_dir)?,
        journal: catalog_reset_path_evidence(&paths.journal_dir)?,
        tape_cache: catalog_reset_path_evidence(&paths.tape_cache_dir)?,
    })
}

fn catalog_reset_path_evidence(path: &Path) -> Result<CatalogResetPathEvidence, StateError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|err| StateError::io_at("stat catalog reset path", path, err))?;
    let canonical_path = fs::canonicalize(path)
        .map_err(|err| StateError::io_at("canonicalize catalog reset path", path, err))?;
    Ok(CatalogResetPathEvidence {
        configured_path: path.to_path_buf(),
        canonical_path,
        configured_path_is_symlink: metadata.file_type().is_symlink(),
    })
}

fn catalog_reset_preflight_tape(tape: TapeRecord) -> Result<CatalogResetPreflightTape, StateError> {
    let tape_uuid: [u8; 16] = tape.tape_uuid.as_slice().try_into().map_err(|_| {
        StateError::IndexCorrupt(format!(
            "catalog reset preflight tape uuid has length {}, expected 16",
            tape.tape_uuid.len()
        ))
    })?;
    if let Some(voltag) = tape.voltag.as_deref() {
        if voltag.is_empty() || voltag.trim() != voltag {
            return Err(StateError::IndexCorrupt(format!(
                "catalog reset preflight tape {} has invalid voltag {voltag:?}",
                hex_tape_uuid(tape_uuid)
            )));
        }
    }
    if !matches!(tape.kind.as_str(), "data" | "cleaning") {
        return Err(StateError::IndexCorrupt(format!(
            "catalog reset preflight tape {} has invalid kind {:?}",
            hex_tape_uuid(tape_uuid),
            tape.kind
        )));
    }
    if !matches!(
        tape.state.as_str(),
        "ready"
            | "ingested"
            | "sealed"
            | "retired"
            | "finalizing"
            | "finalized"
            | "finalized_degraded"
            | "recovery_required"
    ) {
        return Err(StateError::IndexCorrupt(format!(
            "catalog reset preflight tape {} has invalid state {:?}",
            hex_tape_uuid(tape_uuid),
            tape.state
        )));
    }
    match tape.scheme_id.as_deref() {
        None => {
            if tape.data_blocks_per_stripe.is_some()
                || tape.parity_blocks_per_stripe.is_some()
                || tape.stripes_per_neighborhood.is_some()
            {
                return Err(StateError::IndexCorrupt(format!(
                    "catalog reset preflight tape {} has partial parity geometry",
                    hex_tape_uuid(tape_uuid)
                )));
            }
        }
        Some(scheme_id) => {
            let scheme = remanence_parity::ParityScheme {
                id: remanence_parity::SchemeId::new_owned(scheme_id.to_string()),
                data_blocks_per_stripe: u16::try_from(tape.data_blocks_per_stripe.ok_or_else(
                    || {
                        StateError::IndexCorrupt(
                            "catalog reset preflight parity geometry is missing data width"
                                .to_string(),
                        )
                    },
                )?)
                .map_err(|_| {
                    StateError::IndexCorrupt(
                        "catalog reset preflight parity data width exceeds u16".to_string(),
                    )
                })?,
                parity_blocks_per_stripe: u16::try_from(tape.parity_blocks_per_stripe.ok_or_else(
                    || {
                        StateError::IndexCorrupt(
                            "catalog reset preflight parity geometry is missing parity width"
                                .to_string(),
                        )
                    },
                )?)
                .map_err(|_| {
                    StateError::IndexCorrupt(
                        "catalog reset preflight parity width exceeds u16".to_string(),
                    )
                })?,
                stripes_per_neighborhood: tape.stripes_per_neighborhood.ok_or_else(|| {
                    StateError::IndexCorrupt(
                        "catalog reset preflight parity geometry is missing neighborhood width"
                            .to_string(),
                    )
                })?,
            };
            scheme.validate().map_err(|error| {
                StateError::IndexCorrupt(format!(
                    "catalog reset preflight tape {} has invalid parity geometry: {error}",
                    hex_tape_uuid(tape_uuid)
                ))
            })?;
        }
    }
    if tape.kind == "data" && tape.block_size.is_none() {
        return Err(StateError::IndexCorrupt(format!(
            "catalog reset preflight data tape {} has no block size",
            hex_tape_uuid(tape_uuid)
        )));
    }
    if tape.block_size == Some(0) || tape.block_size.is_some_and(|value| value > u32::MAX.into()) {
        return Err(StateError::IndexCorrupt(format!(
            "catalog reset preflight tape {} has invalid block size {:?}",
            hex_tape_uuid(tape_uuid),
            tape.block_size
        )));
    }
    Ok(CatalogResetPreflightTape {
        tape_uuid,
        voltag: tape.voltag,
        kind: tape.kind,
        pool_id: tape.pool_id,
        assignment_generation: tape.assignment_generation,
        state: tape.state,
        block_size: tape.block_size,
        scheme_id: tape.scheme_id,
        data_blocks_per_stripe: tape.data_blocks_per_stripe,
        parity_blocks_per_stripe: tape.parity_blocks_per_stripe,
        stripes_per_neighborhood: tape.stripes_per_neighborhood,
    })
}

const CATALOG_RESET_FENCE_FILE: &str = "catalog-reset.in-progress";
const CATALOG_RESET_FENCE_MAGIC: &str = "REM-CATALOG-RESET-V2";

fn load_catalog_reset_config_snapshot(path: &Path) -> Result<(RemConfig, Vec<u8>), StateError> {
    let bytes = fs::read(path).map_err(|err| StateError::io_at("read config", path, err))?;
    let text = std::str::from_utf8(&bytes).map_err(|err| {
        StateError::ConfigInvalid(format!("config {} is not UTF-8: {err}", path.display()))
    })?;
    let config = parse_config_toml(text)?;
    Ok((config, bytes))
}

fn ensure_catalog_reset_config_unchanged(
    paths: &StatePaths,
    initially_parsed: &RemConfig,
    locked_snapshot: &RemConfig,
) -> Result<(), StateError> {
    let locked_paths = StatePaths::from_config(&paths.config_path, locked_snapshot);
    if initially_parsed != locked_snapshot || &locked_paths != paths {
        return Err(StateError::ConfigInvalid(
            "catalog reset config changed before the exclusive admission lock; retry from preflight"
                .to_string(),
        ));
    }
    Ok(())
}

fn catalog_reset_request_digest(
    paths: &StatePaths,
    config_bytes: &[u8],
    preserve: &[String],
    allow_erase: &[String],
    ordinary_full_reset: bool,
    preflight_token: &str,
) -> Result<String, StateError> {
    let config_path = fs::canonicalize(&paths.config_path).map_err(|err| {
        StateError::io_at(
            "canonicalize catalog reset config path",
            &paths.config_path,
            err,
        )
    })?;
    let mut preserve = preserve.to_vec();
    let mut allow_erase = allow_erase.to_vec();
    preserve.sort();
    allow_erase.sort();
    let mut hasher = Sha256::new();
    hash_catalog_reset_field(
        &mut hasher,
        if ordinary_full_reset {
            b"ordinary-full".as_slice()
        } else {
            b"scoped".as_slice()
        },
    );
    hash_catalog_reset_field(&mut hasher, config_path.as_os_str().as_encoded_bytes());
    hash_catalog_reset_field(&mut hasher, config_bytes);
    hash_catalog_reset_field(&mut hasher, preflight_token.as_bytes());
    for voltag in preserve {
        hash_catalog_reset_field(&mut hasher, b"preserve");
        hash_catalog_reset_field(&mut hasher, voltag.as_bytes());
    }
    for voltag in allow_erase {
        hash_catalog_reset_field(&mut hasher, b"allow-erase");
        hash_catalog_reset_field(&mut hasher, voltag.as_bytes());
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn catalog_reset_preflight_token(
    paths: &StatePaths,
    config_bytes: &[u8],
    preserve: &[String],
    allow_erase: &[String],
    source_schema_version: u32,
    tapes: &[CatalogResetPreflightTape],
    preserved: &[CatalogResetPreservedTape],
) -> Result<String, StateError> {
    let path_evidence = catalog_reset_preflight_paths(paths)?;
    let mut hasher = Sha256::new();
    hash_catalog_reset_field(&mut hasher, b"REM-CATALOG-PREFLIGHT-V1");
    hash_catalog_reset_field(&mut hasher, &source_schema_version.to_be_bytes());
    hash_catalog_reset_field(&mut hasher, config_bytes);
    for path in [
        &path_evidence.config,
        &path_evidence.state_dir,
        &path_evidence.sqlite,
        &path_evidence.audit,
        &path_evidence.journal,
        &path_evidence.tape_cache,
    ] {
        hash_catalog_reset_field(
            &mut hasher,
            path.configured_path.as_os_str().as_encoded_bytes(),
        );
        hash_catalog_reset_field(
            &mut hasher,
            path.canonical_path.as_os_str().as_encoded_bytes(),
        );
        hash_catalog_reset_field(
            &mut hasher,
            if path.configured_path_is_symlink {
                b"symlink"
            } else {
                b"direct"
            },
        );
    }
    let mut preserve = preserve.to_vec();
    let mut allow_erase = allow_erase.to_vec();
    preserve.sort();
    allow_erase.sort();
    for value in preserve {
        hash_catalog_reset_field(&mut hasher, b"preserve");
        hash_catalog_reset_field(&mut hasher, value.as_bytes());
    }
    for value in allow_erase {
        hash_catalog_reset_field(&mut hasher, b"allow-erase");
        hash_catalog_reset_field(&mut hasher, value.as_bytes());
    }
    let restore_ready = preserved
        .iter()
        .map(|tape| (tape.tape_uuid, tape.restore_ready))
        .collect::<BTreeMap<_, _>>();
    let mut tapes = tapes.to_vec();
    tapes.sort_by_key(|tape| tape.tape_uuid);
    for tape in tapes {
        hash_catalog_reset_field(&mut hasher, &tape.tape_uuid);
        for value in [
            tape.voltag.as_deref(),
            Some(tape.kind.as_str()),
            tape.pool_id.as_deref(),
            Some(tape.state.as_str()),
            tape.scheme_id.as_deref(),
        ] {
            match value {
                Some(value) => {
                    hash_catalog_reset_field(&mut hasher, b"some");
                    hash_catalog_reset_field(&mut hasher, value.as_bytes());
                }
                None => hash_catalog_reset_field(&mut hasher, b"none"),
            }
        }
        for value in [
            tape.block_size,
            Some(tape.assignment_generation),
            tape.data_blocks_per_stripe.map(u64::from),
            tape.parity_blocks_per_stripe.map(u64::from),
            tape.stripes_per_neighborhood.map(u64::from),
        ] {
            match value {
                Some(value) => hash_catalog_reset_field(&mut hasher, &value.to_be_bytes()),
                None => hash_catalog_reset_field(&mut hasher, b"none"),
            }
        }
        hash_catalog_reset_field(
            &mut hasher,
            if restore_ready.get(&tape.tape_uuid).copied().unwrap_or(false) {
                b"restore-ready"
            } else {
                b"not-restore-ready"
            },
        );
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn hash_catalog_reset_output_tape(
    hasher: &mut Sha256,
    tape: &CatalogResetPreflightTape,
    pool_id: Option<&str>,
    assignment_generation: u64,
    state: &str,
) {
    hash_catalog_reset_field(hasher, &tape.tape_uuid);
    for value in [
        tape.voltag.as_deref(),
        Some(tape.kind.as_str()),
        pool_id,
        Some(state),
        tape.scheme_id.as_deref(),
    ] {
        match value {
            Some(value) => {
                hash_catalog_reset_field(hasher, b"some");
                hash_catalog_reset_field(hasher, value.as_bytes());
            }
            None => hash_catalog_reset_field(hasher, b"none"),
        }
    }
    for value in [
        tape.block_size,
        Some(assignment_generation),
        tape.data_blocks_per_stripe.map(u64::from),
        tape.parity_blocks_per_stripe.map(u64::from),
        tape.stripes_per_neighborhood.map(u64::from),
    ] {
        match value {
            Some(value) => {
                hash_catalog_reset_field(hasher, b"some");
                hash_catalog_reset_field(hasher, &value.to_be_bytes());
            }
            None => hash_catalog_reset_field(hasher, b"none"),
        }
    }
}

fn catalog_reset_intended_output_token(
    tapes: &[CatalogResetPreflightTape],
    preserved: &[CatalogResetPreservedTape],
) -> Result<String, StateError> {
    let by_uuid = tapes
        .iter()
        .map(|tape| (tape.tape_uuid, tape))
        .collect::<BTreeMap<_, _>>();
    let mut preserved = preserved.to_vec();
    preserved.sort_by_key(|tape| tape.tape_uuid);
    let mut hasher = Sha256::new();
    hash_catalog_reset_field(&mut hasher, b"REM-CATALOG-RESET-OUTPUT-V1");
    for tape in preserved {
        let source = by_uuid.get(&tape.tape_uuid).ok_or_else(|| {
            StateError::IndexCorrupt("preserved tape missing from preflight rows".to_string())
        })?;
        let generation = source.assignment_generation.checked_add(1).ok_or_else(|| {
            StateError::IndexCorrupt("catalog reset assignment generation exhausted".to_string())
        })?;
        hash_catalog_reset_output_tape(
            &mut hasher,
            source,
            tape.pool_id.as_deref(),
            generation,
            if tape.restore_ready {
                "ready"
            } else {
                "recovery_required"
            },
        );
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn catalog_reset_current_output_token(tapes: &[CatalogResetPreflightTape]) -> String {
    let mut tapes = tapes.to_vec();
    tapes.sort_by_key(|tape| tape.tape_uuid);
    let mut hasher = Sha256::new();
    hash_catalog_reset_field(&mut hasher, b"REM-CATALOG-RESET-OUTPUT-V1");
    for tape in tapes {
        hash_catalog_reset_output_tape(
            &mut hasher,
            &tape,
            tape.pool_id.as_deref(),
            tape.assignment_generation,
            tape.state.as_str(),
        );
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hash_catalog_reset_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn catalog_reset_fence_path(paths: &StatePaths) -> PathBuf {
    paths.state_dir.join(CATALOG_RESET_FENCE_FILE)
}

fn ensure_no_catalog_reset_fence(paths: &StatePaths) -> Result<(), StateError> {
    let fence = catalog_reset_fence_path(paths);
    if fence.exists() {
        return Err(StateError::CatalogResetInProgress(format!(
            "durable fence {} requires an exact reset rerun",
            fence.display()
        )));
    }
    Ok(())
}

fn catalog_reset_fence_evidence(
    paths: &StatePaths,
) -> Result<Option<(String, String, String)>, StateError> {
    let fence = catalog_reset_fence_path(paths);
    if !fence.exists() {
        return Ok(None);
    }
    let observed = fs::read_to_string(&fence)
        .map_err(|err| StateError::io_at("read catalog reset fence", &fence, err))?;
    let lines = observed.lines().collect::<Vec<_>>();
    let valid_hex = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    if lines.len() != 4
        || lines[0] != CATALOG_RESET_FENCE_MAGIC
        || !valid_hex(lines[1])
        || !valid_hex(lines[2])
        || !valid_hex(lines[3])
    {
        Err(StateError::CatalogResetInProgress(format!(
            "fence {} is corrupt",
            fence.display()
        )))
    } else {
        Ok(Some((
            lines[1].to_string(),
            lines[2].to_string(),
            lines[3].to_string(),
        )))
    }
}

fn catalog_reset_fence_admits_request(
    paths: &StatePaths,
    request_digest: &str,
    preflight_token: &str,
    output_token: &str,
) -> Result<bool, StateError> {
    let Some((stored_request, stored_token, stored_output)) = catalog_reset_fence_evidence(paths)?
    else {
        return Ok(false);
    };
    if stored_request == request_digest
        && stored_token == preflight_token
        && stored_output == output_token
    {
        Ok(true)
    } else {
        Err(StateError::CatalogResetInProgress(format!(
            "fence {} belongs to a different reset request or preflight token",
            catalog_reset_fence_path(paths).display()
        )))
    }
}

fn begin_or_resume_catalog_reset_fence(
    paths: &StatePaths,
    request_digest: &str,
    preflight_token: &str,
    output_token: &str,
) -> Result<(), StateError> {
    let fence = catalog_reset_fence_path(paths);
    if catalog_reset_fence_admits_request(paths, request_digest, preflight_token, output_token)? {
        return Ok(());
    }
    let expected = format!(
        "{CATALOG_RESET_FENCE_MAGIC}\n{request_digest}\n{preflight_token}\n{output_token}\n"
    );
    let temporary = paths
        .state_dir
        .join(format!("{CATALOG_RESET_FENCE_FILE}.new"));
    remove_file_if_exists(&temporary)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    match options.open(&temporary) {
        Ok(mut file) => {
            if let Err(err) = file
                .write_all(expected.as_bytes())
                .and_then(|()| file.sync_all())
            {
                let _ = remove_file_if_exists(&temporary);
                return Err(StateError::io_at(
                    "write catalog reset fence",
                    &temporary,
                    err,
                ));
            }
            drop(file);
            fs::rename(&temporary, &fence)
                .map_err(|err| StateError::io_at("install catalog reset fence", &fence, err))?;
            sync_directory(&paths.state_dir)?;
            Ok(())
        }
        Err(err) => Err(StateError::io_at(
            "create catalog reset fence",
            &temporary,
            err,
        )),
    }
}

fn clear_catalog_reset_fence(paths: &StatePaths) -> Result<(), StateError> {
    let fence = catalog_reset_fence_path(paths);
    remove_file_if_exists(&fence)?;
    sync_directory(&paths.state_dir)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatalogResetPhase {
    AuthoritativeInputsArchived,
    SourceCheckpointed,
    ReplacementSchemaCreated,
    PoolsProjected,
    TapesRestored,
    AuthoritativeInputsCleared,
    BeforeAtomicSwap,
    AfterAtomicSwap,
}

#[derive(Clone, Copy)]
struct CatalogResetAdmission<'a> {
    request_digest: &'a str,
    preflight_token: &'a str,
    output_token: &'a str,
    source_schema_version: Option<u32>,
}

fn reset_catalog_locked(
    paths: &StatePaths,
    config: &RemConfig,
    preserved_tapes: &[CatalogResetPreservedTape],
    admission: CatalogResetAdmission<'_>,
) -> Result<(), StateError> {
    reset_catalog_locked_with_hook(paths, config, preserved_tapes, admission, |_| Ok(()))
}

fn reset_catalog_locked_with_hook<F>(
    paths: &StatePaths,
    config: &RemConfig,
    preserved_tapes: &[CatalogResetPreservedTape],
    admission: CatalogResetAdmission<'_>,
    mut phase_hook: F,
) -> Result<(), StateError>
where
    F: FnMut(CatalogResetPhase) -> Result<(), StateError>,
{
    begin_or_resume_catalog_reset_fence(
        paths,
        admission.request_digest,
        admission.preflight_token,
        admission.output_token,
    )?;
    archive_reset_authoritative_inputs(paths)?;
    phase_hook(CatalogResetPhase::AuthoritativeInputsArchived)?;
    ensure_state_directories(paths)?;

    // The source remains a complete, reusable catalog until the final atomic
    // rename. Checkpointing first makes removal of its stale sidecars safe.
    if paths.sqlite_path.exists() {
        if let Some(expected_schema_version) = admission.source_schema_version {
            CatalogIndex::prepare_admitted_catalog_reset_source_atomic_swap(
                &paths.sqlite_path,
                expected_schema_version,
            )?;
        } else {
            let source = CatalogIndex::open(&paths.sqlite_path)?;
            source.prepare_catalog_reset_atomic_swap()?;
        }
    }
    phase_hook(CatalogResetPhase::SourceCheckpointed)?;

    let replacement_path = create_unique_reset_sqlite_path(&paths.sqlite_path)?;
    let replacement_result = (|| {
        let mut replacement = CatalogIndex::open(&replacement_path)?;
        phase_hook(CatalogResetPhase::ReplacementSchemaCreated)?;
        let _config_warnings = project_configured_tape_pools(&mut replacement, config)?;
        replacement.reconcile_cleaning_prefixes(&config.cleaning.voltag_prefixes)?;
        phase_hook(CatalogResetPhase::PoolsProjected)?;
        replacement.restore_catalog_reset_tapes(preserved_tapes)?;
        phase_hook(CatalogResetPhase::TapesRestored)?;
        replacement.prepare_catalog_reset_atomic_swap()?;
        drop(replacement);
        sync_regular_file(&replacement_path)?;

        // `paths.calibration_dir` is deliberately NOT reset. The durable
        // allocator advances while every evicted projection disappears.
        let calibration = CalibrationControlStore::open(&paths.calibration_dir)?;
        calibration.record_all_maps_evicted("catalog_reset")?;
        reset_directory_contents(&paths.audit_dir)?;
        reset_directory_contents(&paths.journal_dir)?;
        reset_directory_contents(&paths.tape_cache_dir)?;
        phase_hook(CatalogResetPhase::AuthoritativeInputsCleared)?;

        // The source main file was checkpointed above. Its sidecars can now be
        // removed without making the source unusable if the process stops
        // before the atomic rename.
        remove_sqlite_sidecars(&paths.sqlite_path)?;
        phase_hook(CatalogResetPhase::BeforeAtomicSwap)?;
        fs::rename(&replacement_path, &paths.sqlite_path).map_err(|err| {
            StateError::io_at("atomically replace catalog sqlite", &paths.sqlite_path, err)
        })?;
        if let Some(parent) = paths.sqlite_path.parent() {
            sync_directory(parent)?;
        }
        phase_hook(CatalogResetPhase::AfterAtomicSwap)?;
        clear_catalog_reset_fence(paths)?;
        Ok(())
    })();
    if replacement_result.is_err() {
        let _ = remove_file_if_exists(&replacement_path);
        let _ = remove_sqlite_sidecars(&replacement_path);
    }
    replacement_result
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
    create_private_dir(&paths.calibration_dir)?;
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
    sync_directory(&archive_dir)?;
    if let Some(root) = archive_dir.parent() {
        sync_directory(root)?;
    }
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
            sync_regular_file(&destination_path)?;
        } else {
            return Err(StateError::ConfigInvalid(format!(
                "refusing to archive non-file state entry {}",
                source_path.display()
            )));
        }
    }
    sync_directory(destination)?;
    Ok(())
}

fn create_unique_reset_sqlite_path(sqlite_path: &Path) -> Result<PathBuf, StateError> {
    let parent = sqlite_path.parent().ok_or_else(|| {
        StateError::ConfigInvalid(format!(
            "catalog sqlite path {} has no parent directory",
            sqlite_path.display()
        ))
    })?;
    let filename = sqlite_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StateError::ConfigInvalid(format!(
                "catalog sqlite path {} has no UTF-8 filename",
                sqlite_path.display()
            ))
        })?;
    for index in 1..=999_999u32 {
        let candidate = parent.join(format!(".{filename}.reset-{index:06}"));
        if !candidate.exists()
            && !sqlite_sidecar_path(&candidate, "wal").exists()
            && !sqlite_sidecar_path(&candidate, "shm").exists()
        {
            return Ok(candidate);
        }
    }
    Err(StateError::ConfigInvalid(format!(
        "no free catalog reset replacement name beside {}",
        sqlite_path.display()
    )))
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    path.with_file_name(format!(
        "{}-{suffix}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rem-state.sqlite")
    ))
}

fn remove_sqlite_sidecars(path: &Path) -> Result<(), StateError> {
    remove_file_if_exists(&sqlite_sidecar_path(path, "wal"))?;
    remove_file_if_exists(&sqlite_sidecar_path(path, "shm"))?;
    remove_file_if_exists(&sqlite_sidecar_path(path, "journal"))?;
    Ok(())
}

fn sync_regular_file(path: &Path) -> Result<(), StateError> {
    fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|err| StateError::io_at("fsync catalog reset replacement", path, err))
}

fn sync_directory(path: &Path) -> Result<(), StateError> {
    #[cfg(unix)]
    {
        fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|err| StateError::io_at("fsync catalog directory", path, err))?;
    }

    #[cfg(not(unix))]
    let _ = path;

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
    use crate::index::{StoredWrapDescriptor, TapeKindFilter, WrapMapCacheRecord};

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

    fn seed_clean_break_reset_catalog(
        root: &Path,
        schema_version: u32,
        tapes: &[([u8; 16], &str)],
    ) {
        for path in [
            root.join("index"),
            root.join("audit"),
            root.join("journals"),
            root.join("cache/tapes"),
        ] {
            fs::create_dir_all(path).expect("create legacy reset path");
        }
        let conn = rusqlite::Connection::open(root.join("index/rem-state.sqlite"))
            .expect("open legacy reset catalog");
        conn.execute_batch(
            "create table tapes(
               tape_uuid blob primary key,
               voltag text,
               pool_id text,
               kind text not null default 'data',
               cleaning_uses integer,
               cleaning_state text,
               block_size integer,
               scheme_id text,
               data_blocks_per_stripe integer,
               parity_blocks_per_stripe integer,
               stripes_per_neighborhood integer,
               highest_protected_ordinal integer not null default 0,
               total_committed_ordinals integer not null default 0,
               last_committed_tape_file integer,
               written_extent_lba integer,
               state text not null,
               updated_at_utc text not null
             );",
        )
        .expect("create schema-16 tapes table");
        for (tape_uuid, voltag) in tapes {
            conn.execute(
                "insert into tapes(
                   tape_uuid, voltag, pool_id, kind, block_size, state, updated_at_utc
                 ) values(?1, ?2, 'camera.copy-a', 'data', 1048576, 'ready',
                          '2026-08-09T00:00:00Z')",
                rusqlite::params![tape_uuid.as_slice(), voltag],
            )
            .expect("insert schema-16 tape");
        }
        conn.pragma_update(None, "user_version", schema_version)
            .expect("mark clean-break catalog version");
    }

    fn adoption_input(operation_id: Uuid) -> AdoptBootstrapIdentityInput {
        AdoptBootstrapIdentityInput {
            operation_id,
            tape_uuid: [
                0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x46, 0x17, 0x88, 0x19, 0x1a, 0x1b, 0x1c, 0x1d,
                0x1e, 0x1f,
            ],
            voltag: "ACM901L9".to_string(),
            block_size: 1024 * 1024,
            parity: ParityConfig::Scheme(remanence_parity::default_scheme_for_block_size(
                1024 * 1024,
            )),
        }
    }

    fn adoption_evidence(tail: BootstrapAdoptionTailEvidence) -> BootstrapAdoptionEvidence {
        BootstrapAdoptionEvidence {
            library_serial: "LIB001".to_string(),
            library_revision: "D.00".to_string(),
            home_slot: 0x0401,
            drive_element: 0x0101,
            drive_serial: "DRV001".to_string(),
            bootstrap_drive_compression: false,
            configured_drive_compression: false,
            physical_tail: tail,
        }
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
    fn bootstrap_adoption_is_audited_identity_only_and_idempotent() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-adopt-bootstrap")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let operation_id = Uuid::new_v4();
        let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");

        let first = handle
            .adopt_bootstrap_identity(
                adoption_input(operation_id),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect("adopt Bootstrap");
        let second = handle
            .adopt_bootstrap_identity(
                adoption_input(operation_id),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect("repeat adoption");
        let fresh_operation_id = Uuid::new_v4();
        let mut fresh_evidence =
            adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod);
        fresh_evidence.home_slot = 0x0402;
        let fresh_operation = handle
            .adopt_bootstrap_identity(adoption_input(fresh_operation_id), fresh_evidence)
            .expect("fresh operation may confirm exact identity-only adoption");

        assert!(first.newly_adopted);
        assert!(!second.newly_adopted);
        assert!(!fresh_operation.newly_adopted);
        assert_eq!(first.operation_id, operation_id);
        assert_eq!(first.request_fingerprint, second.request_fingerprint);
        assert_eq!(fresh_operation.operation_id, fresh_operation_id);
        assert_ne!(
            first.request_fingerprint,
            fresh_operation.request_fingerprint
        );
        assert_eq!(first.assignment_generation, 1);
        assert_eq!(first.state, AdoptedTapeState::Ready);
        let tape = handle
            .catalog_index()
            .get_tape(&adoption_input(operation_id).tape_uuid)
            .expect("lookup")
            .expect("adopted row");
        assert_eq!(tape.state, "ready");
        assert_eq!(tape.pool_id.as_deref(), Some("camera.copy-a"));
        assert_eq!(tape.total_committed_ordinals, 0);
        assert!(handle
            .catalog_index()
            .list_tape_files(&adoption_input(operation_id).tape_uuid)
            .expect("list tape files")
            .is_empty());
        let records = FileAuditLog::replay(&handle.paths().audit_dir).expect("replay audit");
        let adoption_records: Vec<_> = records
            .iter()
            .filter(|record| record.event == AuditEvent::TapeIdentityAdopted)
            .collect();
        assert_eq!(adoption_records.len(), 1);
        assert_eq!(adoption_records[0].operation_id, Some(operation_id));
        assert_eq!(adoption_records[0].idempotency_key, Some(operation_id));

        handle
            .catalog_index()
            .record_media_readiness_operation(crate::index::MediaReadinessOperationInput {
                operation_id: Uuid::new_v4(),
                run_id: None,
                library_serial: "LIB001".to_string(),
                changer_sg: None,
                drive_element: 0x0101,
                drive_sg: None,
                drive_serial: Some("DRV001".to_string()),
                barcode: Some("ACM901L9".to_string()),
                source_slot: Some(0x0401),
                media_generation: Some(9),
                phase: "planned".to_string(),
                state: "planned".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .expect("record active physical operation guard");
        let guarded = handle
            .adopt_bootstrap_identity(
                adoption_input(Uuid::new_v4()),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("active physical operation evidence must forbid adoption no-op");
        assert!(guarded.to_string().contains("conflicts with existing"));
    }

    #[test]
    fn bootstrap_adoption_recovers_audit_before_projection_crash_cut() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-adopt-crash-cut")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let operation_id = Uuid::new_v4();
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .adopt_bootstrap_identity(
                    adoption_input(operation_id),
                    adoption_evidence(BootstrapAdoptionTailEvidence::DataAfterBootstrap),
                )
                .expect("initial adoption");
        }
        let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open sqlite");
        conn.execute(
            "delete from tapes where tape_uuid = ?1",
            rusqlite::params![adoption_input(operation_id).tape_uuid.to_vec()],
        )
        .expect("simulate missing projection");
        drop(conn);

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        let recovery_operation_id = Uuid::new_v4();
        let outcome = handle
            .adopt_bootstrap_identity(
                adoption_input(recovery_operation_id),
                adoption_evidence(BootstrapAdoptionTailEvidence::DataAfterBootstrap),
            )
            .expect("resume adoption");
        assert!(!outcome.newly_adopted);
        assert_eq!(outcome.operation_id, recovery_operation_id);
        assert_eq!(outcome.state, AdoptedTapeState::RecoveryRequired);
        assert_eq!(
            handle
                .catalog_index()
                .get_tape(&adoption_input(operation_id).tape_uuid)
                .expect("lookup")
                .expect("reprojected row")
                .state,
            "recovery_required"
        );
    }

    #[test]
    fn bootstrap_adoption_replay_preserves_newer_conflicting_barcode_authority() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-adopt-replay-barcode-conflict")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let original_operation_id = Uuid::new_v4();
        let original = adoption_input(original_operation_id);
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .adopt_bootstrap_identity(
                    original.clone(),
                    adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
                )
                .expect("initial adoption");
        }

        let conflicting_uuid = *Uuid::new_v4().as_bytes();
        let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open sqlite");
        conn.execute(
            "delete from tapes where tape_uuid = ?1",
            rusqlite::params![original.tape_uuid.to_vec()],
        )
        .expect("simulate missing original projection");
        conn.execute(
            "insert into tapes(
               tape_uuid, voltag, pool_id, assignment_generation, kind,
               block_size, highest_protected_ordinal, total_committed_ordinals,
               written_extent_lba, state, updated_at_utc
             ) values(
               ?1, ?2, 'camera.copy-a', 2, 'data', 1048576,
               X'0000000000000000', X'0000000000000000',
               X'0000000000000001', 'ready', '2026-08-09T00:00:00Z'
             )",
            rusqlite::params![conflicting_uuid.to_vec(), original.voltag.as_str()],
        )
        .expect("insert newer conflicting barcode authority");
        drop(conn);

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        let error = handle
            .adopt_bootstrap_identity(
                adoption_input(Uuid::new_v4()),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("old adoption replay must not displace newer barcode authority");
        assert!(error.to_string().contains("conflicts with existing"));

        let conflicting = handle
            .catalog_index()
            .get_tape(&conflicting_uuid)
            .expect("lookup conflicting row")
            .expect("conflicting row remains");
        assert_eq!(
            conflicting.voltag.as_deref(),
            Some(original.voltag.as_str())
        );
        assert_eq!(conflicting.written_extent_lba, Some(1));
        assert!(handle
            .catalog_index()
            .get_tape(&original.tape_uuid)
            .expect("lookup original projection")
            .is_none());
    }

    #[test]
    fn bootstrap_adoption_refuses_reused_operation_and_non_v4_identity() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-adopt-conflict")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let operation_id = Uuid::new_v4();
        let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
        handle
            .adopt_bootstrap_identity(
                adoption_input(operation_id),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect("initial adoption");

        let changed = handle
            .adopt_bootstrap_identity(
                adoption_input(operation_id),
                adoption_evidence(BootstrapAdoptionTailEvidence::DataAfterBootstrap),
            )
            .expect_err("changed tail under operation must fail");
        assert!(changed.to_string().contains("changed immutable facts"));

        let mut changed_drive =
            adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod);
        changed_drive.drive_element = 0x0102;
        changed_drive.drive_serial = "DRV002".to_string();
        let changed_drive = handle
            .adopt_bootstrap_identity(adoption_input(operation_id), changed_drive)
            .expect_err("changed drive provenance under operation must fail");
        assert!(changed_drive
            .to_string()
            .contains("changed immutable facts"));

        let changed_under_fresh_operation = handle
            .adopt_bootstrap_identity(
                adoption_input(Uuid::new_v4()),
                adoption_evidence(BootstrapAdoptionTailEvidence::DataAfterBootstrap),
            )
            .expect_err("fresh operation cannot change the tail-derived catalog state");
        assert!(changed_under_fresh_operation
            .to_string()
            .contains("conflicts with existing"));

        let mut different_tape_same_operation = adoption_input(operation_id);
        different_tape_same_operation.tape_uuid[15] ^= 0x01;
        different_tape_same_operation.voltag = "ACM902L9".to_string();
        let different_tape_same_operation = handle
            .adopt_bootstrap_identity(
                different_tape_same_operation,
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("one operation UUID cannot bind a different tape");
        assert!(different_tape_same_operation
            .to_string()
            .contains("different TapeIdentityAdopted authority"));

        let mut invalid = adoption_input(Uuid::new_v4());
        invalid.tape_uuid = [0; 16];
        let invalid = handle
            .adopt_bootstrap_identity(
                invalid,
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("nil identity must fail");
        assert!(invalid.to_string().contains("UUIDv4"));
    }

    #[test]
    fn bootstrap_adoption_refuses_after_catalog_authority_evolves() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-adopt-evolved")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let operation_id = Uuid::new_v4();
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .adopt_bootstrap_identity(
                    adoption_input(operation_id),
                    adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
                )
                .expect("initial adoption");
        }
        let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open sqlite");
        conn.execute(
            "update tapes set written_extent_lba = X'0000000000000001' where tape_uuid = ?1",
            rusqlite::params![adoption_input(operation_id).tape_uuid.to_vec()],
        )
        .expect("simulate later physical prefix authority");
        drop(conn);

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        let error = handle
            .adopt_bootstrap_identity(
                adoption_input(Uuid::new_v4()),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("evolved prefix authority must forbid re-adoption");
        assert!(error.to_string().contains("conflicts with existing"));
        assert_eq!(
            handle
                .catalog_index()
                .get_tape(&adoption_input(operation_id).tape_uuid)
                .expect("lookup")
                .expect("evolved row")
                .written_extent_lba,
            Some(1)
        );
    }

    #[test]
    fn bootstrap_adoption_refuses_after_assignment_generation_evolves() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-adopt-generation-evolved")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let operation_id = Uuid::new_v4();
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .adopt_bootstrap_identity(
                    adoption_input(operation_id),
                    adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
                )
                .expect("initial adoption");
        }
        let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open sqlite");
        conn.execute(
            "update tapes set assignment_generation = 2 where tape_uuid = ?1",
            rusqlite::params![adoption_input(operation_id).tape_uuid.to_vec()],
        )
        .expect("simulate later assignment authority");
        drop(conn);

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        let error = handle
            .adopt_bootstrap_identity(
                adoption_input(Uuid::new_v4()),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("evolved assignment generation must forbid re-adoption");
        assert!(error.to_string().contains("assignment generation evolved"));
    }

    #[test]
    fn bootstrap_adoption_checks_uuid_and_barcode_conflicts_across_tape_kinds() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-adopt-kind-conflict")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        {
            StateHandle::open_from_config_file(&config_path).expect("initialize state");
        }
        let input = adoption_input(Uuid::new_v4());
        let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open sqlite");
        conn.execute(
            "insert into tapes(tape_uuid, voltag, kind, cleaning_uses, cleaning_state, state, updated_at_utc)
             values(?1, ?2, 'cleaning', 0, 'unverified', 'ready', '2026-08-09T00:00:00Z')",
            rusqlite::params![input.tape_uuid.to_vec(), input.voltag.as_str()],
        )
        .expect("insert conflicting cleaning identity");
        drop(conn);

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        let error = handle
            .adopt_bootstrap_identity(
                input,
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("cleaning identity conflict must fail");
        assert!(
            error.to_string().contains("cleaning catalog row"),
            "{error}"
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
            .list_tapes(None, TapeKindFilter::Data)
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
    fn scoped_reset_preserves_only_exact_identity_geometry_and_current_pool() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-scoped-reset")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let selected_uuid = [0x91; 16];
        let discarded_uuid = [0x92; 16];
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            for (tape_uuid, voltag) in [(selected_uuid, "ACM001L9"), (discarded_uuid, "ACM002L9")] {
                handle
                    .catalog_index()
                    .provision_tape(crate::index::ProvisionTapeInput {
                        tape_uuid,
                        voltag: voltag.to_string(),
                        block_size: 512 * 1024,
                        parity: ParityConfig::None,
                        force: false,
                    })
                    .expect("provision tape");
            }
        }
        fs::write(temp.path().join("audit/source.remaudit"), b"audit").expect("audit marker");
        fs::write(temp.path().join("journals/source.remjournal"), b"journal")
            .expect("journal marker");
        fs::write(temp.path().join("cache/tapes/source.cache"), b"cache").expect("cache marker");

        let admitted = StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &["ACM001L9".to_string()],
            &["ACM002L9".to_string()],
        )
        .expect("preflight exact source");
        let report = StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
            &config_path,
            &["ACM001L9".to_string()],
            &["ACM002L9".to_string()],
            &admitted.preflight_token,
        )
        .expect("scoped reset");

        assert_eq!(report.preserved_tapes.len(), 1);
        assert_eq!(report.preserved_tapes[0].tape_uuid, selected_uuid);
        assert_eq!(report.preserved_tapes[0].voltag, "ACM001L9");
        assert_eq!(
            report.preserved_tapes[0].pool_id.as_deref(),
            Some("camera.copy-a")
        );
        assert_eq!(
            report.preserved_tapes[0].state,
            CatalogResetTapeState::Ready
        );
        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        let tapes = handle
            .catalog_index()
            .list_tapes(None, TapeKindFilter::All)
            .expect("list tapes");
        assert_eq!(tapes.len(), 1);
        let tape = &tapes[0];
        assert_eq!(tape.tape_uuid, selected_uuid);
        assert_eq!(tape.voltag.as_deref(), Some("ACM001L9"));
        assert_eq!(tape.kind, "data");
        assert_eq!(tape.pool_id.as_deref(), Some("camera.copy-a"));
        assert_eq!(tape.assignment_generation, 1);
        assert_eq!(tape.block_size, Some(512 * 1024));
        assert_eq!(tape.state, "ready");
        assert_eq!(tape.last_committed_tape_file, None);
        assert_eq!(tape.total_committed_ordinals, 0);
        assert_eq!(tape.written_extent_lba, None);
        assert_eq!(tape.terminal_finalization, None);
        assert!(handle
            .catalog_index()
            .get_tape(&discarded_uuid)
            .expect("discarded lookup")
            .is_none());
        drop(handle);

        let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open reset sqlite");
        for table in [
            "tape_files",
            "object_copies",
            "catalog_units",
            "operations",
            "sessions",
            "idempotency_keys",
            "ingested_sources",
        ] {
            let count: u64 = conn
                .query_row(&format!("select count(*) from {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count reset projection");
            assert_eq!(count, 0, "{table}");
        }
        let archive = temp.path().join("reset-archives/reset-000001");
        assert_eq!(
            fs::read(archive.join("audit/source.remaudit")).expect("archived audit"),
            b"audit"
        );
        assert_eq!(
            fs::read(archive.join("journals/source.remjournal")).expect("archived journal"),
            b"journal"
        );
        assert!(!archive.join("cache/tapes/source.cache").exists());
    }

    #[test]
    fn scoped_reset_normalizes_written_and_finalized_tapes_to_recovery_required() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-scoped-reset-written")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let written_uuid = [0x93; 16];
        let finalized_uuid = [0x94; 16];
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            for (tape_uuid, voltag) in [(written_uuid, "ACM003L9"), (finalized_uuid, "ACM004L9")] {
                handle
                    .catalog_index()
                    .provision_tape(crate::index::ProvisionTapeInput {
                        tape_uuid,
                        voltag: voltag.to_string(),
                        block_size: 256 * 1024,
                        parity: ParityConfig::None,
                        force: false,
                    })
                    .expect("provision tape");
            }
            handle
                .catalog_index()
                .seal_tape(written_uuid)
                .expect("seal written tape");
            handle
                .catalog_index()
                .project_terminal_finalization(crate::index::TerminalFinalizationProjectionInput {
                    tape_uuid: finalized_uuid,
                    trigger: crate::checkpoint::TerminalFinalizationTrigger::ReachedLowWatermark,
                    operation_id: None,
                    progress: crate::checkpoint::TerminalFinalizationProgress::AfterReplicaC,
                    edition_digest: [0x31; 32],
                    layout_digest: [0x32; 32],
                    outcome: crate::index::TerminalFinalizationOutcome::Finalized,
                    updated_at_utc: None,
                })
                .expect("finalize tape projection");
        }

        let report = StateHandle::reset_catalog_preserving_from_config_file(
            &config_path,
            &["ACM003L9".to_string(), "ACM004L9".to_string()],
        )
        .expect("scoped reset");
        assert!(report
            .preserved_tapes
            .iter()
            .all(|tape| tape.state == CatalogResetTapeState::RecoveryRequired));

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        for tape_uuid in [written_uuid, finalized_uuid] {
            let tape = handle
                .catalog_index()
                .get_tape(&tape_uuid)
                .expect("get tape")
                .expect("preserved tape");
            assert_eq!(tape.state, "recovery_required");
            assert_eq!(tape.last_committed_tape_file, None);
            assert_eq!(tape.total_committed_ordinals, 0);
            assert_eq!(tape.written_extent_lba, None);
            assert_eq!(tape.terminal_finalization, None);
        }
    }

    #[test]
    fn scoped_reset_drops_live_guards_but_never_restores_their_tapes_ready() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-scoped-reset-guards")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let guarded = [
            ([0xA1; 16], "ACM021L9"),
            ([0xA2; 16], "ACM022L9"),
            ([0xA3; 16], "ACM023L9"),
            ([0xA4; 16], "ACM024L9"),
        ];
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            for (tape_uuid, voltag) in guarded {
                handle
                    .catalog_index()
                    .provision_tape(ProvisionTapeInput {
                        tape_uuid,
                        voltag: voltag.to_string(),
                        block_size: 1024 * 1024,
                        parity: ParityConfig::None,
                        force: false,
                    })
                    .expect("provision guarded tape");
            }
        }
        let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open source sqlite");
        conn.execute(
            "insert into sessions(
               session_id, session_kind, tape_uuid, state, opened_at_utc, updated_at_utc
             ) values('reset-open-session', 'write', ?1, 'open',
                      '2026-08-09T00:00:00Z', '2026-08-09T00:00:00Z')",
            rusqlite::params![guarded[0].0.as_slice()],
        )
        .expect("insert open session");
        conn.execute(
            "insert into tape_io_fences(
               tape_uuid, barcode, state, reason, quarantine_id,
               created_at_utc, updated_at_utc
             ) values(?1, ?2, 'active', 'test', 'reset-active-fence',
                      '2026-08-09T00:00:00Z', '2026-08-09T00:00:00Z')",
            rusqlite::params![guarded[1].0.as_slice(), guarded[1].1],
        )
        .expect("insert active fence");
        conn.execute(
            "insert into wrap_maps(
               tape_uuid, descriptors_json, mapped_extent_lba, write_epoch,
               calibration_generation, harvested_at_utc
             ) values(?1, '[]', 0, 0, 1, '2026-08-09T00:00:00Z')",
            rusqlite::params![guarded[2].0.as_slice()],
        )
        .expect("insert wrap map");
        conn.execute(
            "insert into media_readiness_ops(
               operation_id, library_serial, drive_element, barcode,
               phase, state, dirty_scope, started_at_utc, updated_at_utc
             ) values('reset-readiness', 'LIB-RESET', 1, ?1,
                      'load', 'running', 'drive+tape',
                      '2026-08-09T00:00:00Z', '2026-08-09T00:00:00Z')",
            rusqlite::params![guarded[3].1],
        )
        .expect("insert active readiness operation");
        drop(conn);

        let preserve = guarded
            .iter()
            .map(|(_, voltag)| (*voltag).to_string())
            .collect::<Vec<_>>();
        let report =
            StateHandle::reset_catalog_preserving_from_config_file(&config_path, &preserve)
                .expect("reset guarded tapes");
        assert_eq!(report.preserved_tapes.len(), guarded.len());
        assert!(report
            .preserved_tapes
            .iter()
            .all(|tape| tape.state == CatalogResetTapeState::RecoveryRequired));

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        for (tape_uuid, _) in guarded {
            assert_eq!(
                handle
                    .catalog_index()
                    .get_tape(&tape_uuid)
                    .expect("lookup guarded tape")
                    .expect("preserved tape")
                    .state,
                "recovery_required"
            );
        }
        let reset_conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open replacement sqlite");
        for table in [
            "sessions",
            "tape_io_fences",
            "wrap_maps",
            "media_readiness_ops",
        ] {
            let count: u64 = reset_conn
                .query_row(&format!("select count(*) from {table}"), [], |row| {
                    row.get(0)
                })
                .expect("count discarded guard rows");
            assert_eq!(count, 0, "{table}");
        }
    }

    #[test]
    fn scoped_reset_selector_failures_precede_archive_or_delete() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-scoped-reset-invalid")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .catalog_index()
                .provision_tape(crate::index::ProvisionTapeInput {
                    tape_uuid: [0x95; 16],
                    voltag: "ACM005L9".to_string(),
                    block_size: 256 * 1024,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision tape");
        }
        fs::write(temp.path().join("audit/sentinel"), b"unchanged").expect("write sentinel");

        for selectors in [
            vec!["MISSINGL9".to_string()],
            vec!["ACM005L9".to_string(), "ACM005L9".to_string()],
            vec!["".to_string()],
            vec!["   ".to_string()],
            vec![" ACM005L9".to_string()],
        ] {
            StateHandle::reset_catalog_preserving_from_config_file(&config_path, &selectors)
                .expect_err("invalid scoped reset must fail");
            assert_eq!(
                fs::read(temp.path().join("audit/sentinel")).expect("sentinel survives"),
                b"unchanged"
            );
            assert!(!temp.path().join("reset-archives").exists());
            let index = CatalogIndex::open_read_only(temp.path().join("index/rem-state.sqlite"))
                .expect("catalog survives");
            assert!(index
                .get_tape_by_voltag("ACM005L9")
                .expect("lookup")
                .is_some());
        }

        let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open sqlite");
        conn.execute(
            "update tapes set assignment_generation = ?1 where voltag = 'ACM005L9'",
            rusqlite::params![i64::MAX],
        )
        .expect("exhaust assignment generation");
        drop(conn);
        StateHandle::reset_catalog_preserving_from_config_file(
            &config_path,
            &["ACM005L9".to_string()],
        )
        .expect_err("exhausted assignment generation must fail");
        assert_eq!(
            fs::read(temp.path().join("audit/sentinel")).expect("sentinel survives"),
            b"unchanged"
        );
        assert!(!temp.path().join("reset-archives").exists());
    }

    #[test]
    fn scoped_reset_preflight_token_refuses_source_identity_geometry_state_and_pool_drift() {
        for (label, mutation) in [
            (
                "uuid",
                "update tapes set tape_uuid = X'BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB' where voltag = 'ACM031L9'",
            ),
            (
                "geometry",
                "update tapes set block_size = 524288 where voltag = 'ACM031L9'",
            ),
            (
                "state",
                "update tapes set state = 'sealed' where voltag = 'ACM031L9'",
            ),
            (
                "pool",
                "update tapes set pool_id = null where voltag = 'ACM031L9'",
            ),
        ] {
            let temp = tempfile::Builder::new()
                .prefix(&format!("remanence-state-reset-token-{label}"))
                .tempdir()
                .expect("temp dir");
            let config_path = temp.path().join("config.toml");
            fs::write(&config_path, config_text(temp.path())).expect("write config");
            {
                let mut handle =
                    StateHandle::open_from_config_file(&config_path).expect("open state");
                handle
                    .catalog_index()
                    .provision_tape(ProvisionTapeInput {
                        tape_uuid: [0xBA; 16],
                        voltag: "ACM031L9".to_string(),
                        block_size: 1024 * 1024,
                        parity: ParityConfig::None,
                        force: false,
                    })
                    .expect("provision source");
                handle
                    .catalog_index()
                    .project_tape_pool_membership([0xBA; 16], "camera.copy-a")
                    .expect("assign source pool");
            }
            let admitted = StateHandle::preflight_catalog_reset_from_config_file(
                &config_path,
                &["ACM031L9".to_string()],
                &[],
            )
            .expect("preflight source");
            fs::write(temp.path().join("audit/sentinel"), b"unchanged").expect("sentinel");
            let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
                .expect("open source sqlite");
            conn.execute(mutation, []).expect("mutate source evidence");
            drop(conn);

            let result = StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
                &config_path,
                &["ACM031L9".to_string()],
                &[],
                &admitted.preflight_token,
            );
            let error = match result {
                Err(error) => error,
                Ok(report) => {
                    panic!("{label}: source drift unexpectedly passed: {report:?}")
                }
            };
            assert!(
                matches!(error, StateError::ConfigInvalid(_)),
                "{label}: {error}"
            );
            assert_eq!(
                fs::read(temp.path().join("audit/sentinel")).expect("sentinel survives"),
                b"unchanged",
                "{label}"
            );
            assert!(
                !temp.path().join("reset-archives").exists(),
                "{label}: mutation must not start"
            );
            assert!(!catalog_reset_fence_path(&StatePaths::from_config(
                &config_path,
                &load_config(&config_path).expect("config")
            ))
            .exists());
        }

        #[cfg(unix)]
        {
            let temp = tempfile::Builder::new()
                .prefix("remanence-state-reset-token-symlink")
                .tempdir()
                .expect("temp dir");
            let config_path = temp.path().join("config.toml");
            let config_link = temp.path().join("config-link.toml");
            fs::write(&config_path, config_text(temp.path())).expect("write config");
            {
                let mut handle =
                    StateHandle::open_from_config_file(&config_path).expect("open state");
                handle
                    .catalog_index()
                    .provision_tape(ProvisionTapeInput {
                        tape_uuid: [0xBC; 16],
                        voltag: "ACM032L9".to_string(),
                        block_size: 1024 * 1024,
                        parity: ParityConfig::None,
                        force: false,
                    })
                    .expect("provision source");
            }
            let admitted = StateHandle::preflight_catalog_reset_from_config_file(
                &config_path,
                &["ACM032L9".to_string()],
                &[],
            )
            .expect("direct-path preflight");
            std::os::unix::fs::symlink(&config_path, &config_link).expect("symlink config");
            let error =
                StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
                    &config_link,
                    &["ACM032L9".to_string()],
                    &[],
                    &admitted.preflight_token,
                )
                .expect_err("direct-to-symlink path-shape drift must fail");
            assert!(matches!(error, StateError::ConfigInvalid(_)));
            assert!(!temp.path().join("reset-archives").exists());
        }
    }

    #[test]
    fn scoped_reset_ambiguous_or_incompatible_catalog_fails_before_mutation() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-scoped-reset-corrupt")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .catalog_index()
                .provision_tape(crate::index::ProvisionTapeInput {
                    tape_uuid: [0x96; 16],
                    voltag: "ACM006L9".to_string(),
                    block_size: 256 * 1024,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision tape");
        }
        fs::write(temp.path().join("audit/sentinel"), b"unchanged").expect("write sentinel");
        let sqlite_path = temp.path().join("index/rem-state.sqlite");
        {
            let conn = rusqlite::Connection::open(&sqlite_path).expect("open sqlite");
            conn.execute("drop index tapes_voltag_unique", [])
                .expect("drop unique index");
            conn.execute(
                "insert into tapes(
                   tape_uuid, voltag, kind, block_size,
                   highest_protected_ordinal, total_committed_ordinals,
                   state, updated_at_utc
                 ) values(?1, 'ACM006L9', 'data', 262144,
                          X'0000000000000000', X'0000000000000000',
                          'ready', '2026-08-09T00:00:00Z')",
                rusqlite::params![[0x97_u8; 16].as_slice()],
            )
            .expect("insert ambiguous tape");
        }
        let error = StateHandle::reset_catalog_preserving_from_config_file(
            &config_path,
            &["ACM006L9".to_string()],
        )
        .expect_err("ambiguous catalog must fail");
        assert!(matches!(error, StateError::AmbiguousCatalogLookup(_)));
        assert_eq!(
            fs::read(temp.path().join("audit/sentinel")).expect("sentinel survives"),
            b"unchanged"
        );
        assert!(!temp.path().join("reset-archives").exists());

        let schema_temp = tempfile::Builder::new()
            .prefix("remanence-state-scoped-reset-schema")
            .tempdir()
            .expect("temp dir");
        let schema_config = schema_temp.path().join("config.toml");
        fs::write(&schema_config, config_text(schema_temp.path())).expect("write config");
        {
            let _handle = StateHandle::open_from_config_file(&schema_config).expect("open state");
        }
        fs::write(schema_temp.path().join("audit/sentinel"), b"unchanged").expect("write sentinel");
        let conn = rusqlite::Connection::open(schema_temp.path().join("index/rem-state.sqlite"))
            .expect("open sqlite");
        conn.pragma_update(None, "user_version", crate::index::SCHEMA_VERSION + 1)
            .expect("advance schema");
        drop(conn);
        StateHandle::reset_catalog_preserving_from_config_file(
            &schema_config,
            &["ACM999L9".to_string()],
        )
        .expect_err("incompatible schema must fail");
        assert_eq!(
            fs::read(schema_temp.path().join("audit/sentinel")).expect("sentinel survives"),
            b"unchanged"
        );
        assert!(!schema_temp.path().join("reset-archives").exists());

        let config_temp = tempfile::Builder::new()
            .prefix("remanence-state-scoped-reset-config")
            .tempdir()
            .expect("temp dir");
        let invalid_config_path = config_temp.path().join("config.toml");
        fs::write(&invalid_config_path, config_text(config_temp.path())).expect("write config");
        {
            let _handle = StateHandle::open_from_config_file(&invalid_config_path)
                .expect("create valid source state");
        }
        fs::write(config_temp.path().join("audit/sentinel"), b"unchanged").expect("write sentinel");
        let invalid_config = format!(
            "{}\n[[tape_pools]]\nid = \"camera.copy-a\"\n",
            config_text(config_temp.path())
        );
        fs::write(&invalid_config_path, invalid_config).expect("replace with invalid config");
        StateHandle::reset_catalog_preserving_from_config_file(
            &invalid_config_path,
            &["ACM999L9".to_string()],
        )
        .expect_err("invalid config must fail");
        assert_eq!(
            fs::read(config_temp.path().join("audit/sentinel")).expect("sentinel survives"),
            b"unchanged"
        );
        assert!(!config_temp.path().join("reset-archives").exists());
    }

    #[test]
    fn scoped_reset_interruption_cuts_leave_a_rerunnable_catalog() {
        for cut in [
            CatalogResetPhase::AuthoritativeInputsArchived,
            CatalogResetPhase::SourceCheckpointed,
            CatalogResetPhase::ReplacementSchemaCreated,
            CatalogResetPhase::PoolsProjected,
            CatalogResetPhase::TapesRestored,
            CatalogResetPhase::AuthoritativeInputsCleared,
            CatalogResetPhase::BeforeAtomicSwap,
            CatalogResetPhase::AfterAtomicSwap,
        ] {
            let temp = tempfile::Builder::new()
                .prefix("remanence-state-scoped-reset-cut")
                .tempdir()
                .expect("temp dir");
            let config_path = temp.path().join("config.toml");
            fs::write(&config_path, config_text(temp.path())).expect("write config");
            let tape_uuid = [0x98; 16];
            {
                let mut handle =
                    StateHandle::open_from_config_file(&config_path).expect("open state");
                handle
                    .catalog_index()
                    .provision_tape(crate::index::ProvisionTapeInput {
                        tape_uuid,
                        voltag: "ACM008L9".to_string(),
                        block_size: 1024 * 1024,
                        parity: ParityConfig::None,
                        force: false,
                    })
                    .expect("provision tape");
                handle
                    .catalog_index()
                    .seal_tape(tape_uuid)
                    .expect("seal tape");
            }
            fs::write(temp.path().join("audit/sentinel"), b"audit").expect("write sentinel");
            let admitted = StateHandle::preflight_catalog_reset_from_config_file(
                &config_path,
                &["ACM008L9".to_string()],
                &[],
            )
            .expect("preflight cut fixture");
            let config = load_config(&config_path).expect("load config");
            let paths = StatePaths::from_config(&config_path, &config);
            let config_bytes = fs::read(&config_path).expect("config bytes");
            let request_digest = catalog_reset_request_digest(
                &paths,
                &config_bytes,
                &["ACM008L9".to_string()],
                &[],
                false,
                &admitted.preflight_token,
            )
            .expect("request digest");
            let lock = StateLockGuard::acquire(&paths.state_dir).expect("acquire reset lock");
            let preserved = CatalogIndex::open_read_only(&paths.sqlite_path)
                .expect("open source")
                .capture_catalog_reset_tapes(&["ACM008L9".to_string()], &config.tape_pool_rules)
                .expect("capture source");
            let output_token = catalog_reset_intended_output_token(&admitted.tapes, &preserved)
                .expect("output token");
            let error = reset_catalog_locked_with_hook(
                &paths,
                &config,
                &preserved,
                CatalogResetAdmission {
                    request_digest: &request_digest,
                    preflight_token: &admitted.preflight_token,
                    output_token: &output_token,
                    source_schema_version: Some(admitted.source_schema_version),
                },
                |phase| {
                    if phase == cut {
                        Err(StateError::ConfigInvalid(format!(
                            "injected reset interruption at {phase:?}"
                        )))
                    } else {
                        Ok(())
                    }
                },
            )
            .expect_err("cut must interrupt reset");
            assert!(error.to_string().contains("injected reset interruption"));
            drop(lock);

            let open_error = StateHandle::open_from_config_file(&config_path)
                .expect_err("durable reset fence must block ordinary state open");
            assert!(
                matches!(open_error, StateError::CatalogResetInProgress(_)),
                "{open_error}"
            );

            let source_after_cut = CatalogIndex::open_read_only(&paths.sqlite_path)
                .expect("a complete catalog survives every cut");
            let tape_after_cut = source_after_cut
                .get_tape_by_voltag("ACM008L9")
                .expect("lookup after cut")
                .expect("selected identity survives cut");
            assert_eq!(tape_after_cut.tape_uuid, tape_uuid);
            drop(source_after_cut);

            if cut == CatalogResetPhase::SourceCheckpointed {
                let conn = rusqlite::Connection::open(&paths.sqlite_path).expect("open cut sqlite");
                conn.execute(
                    "update tapes set tape_uuid = X'99999999999999999999999999999999' where voltag = 'ACM008L9'",
                    [],
                )
                .expect("inject pre-swap source drift");
                drop(conn);
                let drift =
                    StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
                        &config_path,
                        &["ACM008L9".to_string()],
                        &[],
                        &admitted.preflight_token,
                    )
                    .expect_err("pre-swap source drift must refuse resume");
                assert!(matches!(drift, StateError::CatalogResetInProgress(_)));
                let conn =
                    rusqlite::Connection::open(&paths.sqlite_path).expect("repair cut sqlite");
                conn.execute(
                    "update tapes set tape_uuid = ?1 where voltag = 'ACM008L9'",
                    rusqlite::params![tape_uuid.as_slice()],
                )
                .expect("remove pre-swap source drift");
                conn.execute(
                    "update tapes set assignment_generation = assignment_generation + 1,
                                      state = 'recovery_required'
                     where voltag = 'ACM008L9'",
                    [],
                )
                .expect("mimic intended tape output before swap");
                drop(conn);
                StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
                    &config_path,
                    &["ACM008L9".to_string()],
                    &[],
                    &admitted.preflight_token,
                )
                .expect_err("nonempty authority directories forbid false post-swap recognition");
                let conn = rusqlite::Connection::open(&paths.sqlite_path)
                    .expect("restore pre-swap source state");
                conn.execute(
                    "update tapes set assignment_generation = assignment_generation - 1,
                                      state = 'sealed'
                     where voltag = 'ACM008L9'",
                    [],
                )
                .expect("restore source state after false-output probe");
            }
            if cut == CatalogResetPhase::AfterAtomicSwap {
                let conn =
                    rusqlite::Connection::open(&paths.sqlite_path).expect("open output sqlite");
                conn.execute(
                    "update tapes set total_committed_ordinals = X'0000000000000001' where voltag = 'ACM008L9'",
                    [],
                )
                .expect("inject stale output counter");
                drop(conn);
                StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
                    &config_path,
                    &["ACM008L9".to_string()],
                    &[],
                    &admitted.preflight_token,
                )
                .expect_err("stale output counter must refuse resume");
                let conn =
                    rusqlite::Connection::open(&paths.sqlite_path).expect("repair output sqlite");
                conn.execute(
                    "update tapes set total_committed_ordinals = X'0000000000000000' where voltag = 'ACM008L9'",
                    [],
                )
                .expect("remove stale output counter");
                conn.execute(
                    "insert into object_files(object_id, file_id, path, size_bytes, file_sha256, chunk_count)
                     values('stale', 'stale', 'stale', X'0000000000000000', X'', X'0000000000000000')",
                    [],
                )
                .expect("inject omitted authority row");
                drop(conn);
                StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
                    &config_path,
                    &["ACM008L9".to_string()],
                    &[],
                    &admitted.preflight_token,
                )
                .expect_err("stale output authority must refuse resume");
                let conn =
                    rusqlite::Connection::open(&paths.sqlite_path).expect("clean output sqlite");
                conn.execute("delete from object_files", [])
                    .expect("remove omitted authority row");
            }

            let preflight = StateHandle::preflight_catalog_reset_from_config_file(
                &config_path,
                &["ACM008L9".to_string()],
                &[],
            )
            .expect("exact preflight admits interrupted reset resume");
            assert!(preflight.resume_exact, "resume status after {cut:?}");
            assert_eq!(preflight.request_digest, request_digest);
            let different = StateHandle::preflight_catalog_reset_from_config_file(
                &config_path,
                &[],
                &["ACM008L9".to_string()],
            )
            .expect_err("different preflight must not cross an existing fence");
            assert!(matches!(different, StateError::CatalogResetInProgress(_)));

            let report = StateHandle::reset_catalog_preserving_from_config_file(
                &config_path,
                &["ACM008L9".to_string()],
            )
            .expect("rerun after interruption");
            assert_eq!(report.preserved_tapes.len(), 1);
            assert_eq!(report.preserved_tapes[0].tape_uuid, tape_uuid);
            assert_eq!(report.preserved_tapes[0].voltag, "ACM008L9");
            assert_eq!(
                report.preserved_tapes[0].state,
                CatalogResetTapeState::RecoveryRequired
            );
            let index_dir = temp.path().join("index");
            let stale_replacements = fs::read_dir(index_dir)
                .expect("read index dir")
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains(".reset-"))
                .count();
            assert_eq!(stale_replacements, 0, "stale replacement after {cut:?}");
            assert!(!catalog_reset_fence_path(&paths).exists());
            let _handle = StateHandle::open_from_config_file(&config_path)
                .expect("ordinary open resumes after exact rerun clears fence");
        }
    }

    #[test]
    fn reset_preflight_lists_all_kinds_and_rechecks_admission_before_mutation() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-reset-preflight")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let data_uuid = [0xA0; 16];
        let cleaning_uuid = [0xA1; 16];
        let erase_uuid = [0xA2; 16];
        let retired_uuid = [0xA3; 16];
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            for (tape_uuid, voltag) in [
                (data_uuid, "ACM010L9"),
                (cleaning_uuid, "CLN010L9"),
                (erase_uuid, "ACM011L9"),
                (retired_uuid, "ACM012L9"),
            ] {
                handle
                    .catalog_index()
                    .provision_tape(crate::index::ProvisionTapeInput {
                        tape_uuid,
                        voltag: voltag.to_string(),
                        block_size: 1024 * 1024,
                        parity: ParityConfig::None,
                        force: false,
                    })
                    .expect("provision tape");
            }
            handle
                .catalog_index()
                .set_tape_kind(&cleaning_uuid, "cleaning")
                .expect("set cleaning kind");
            handle
                .retire_tape(crate::index::RetireTapeInput {
                    tape_uuid: retired_uuid,
                    reason: "test retired history".to_string(),
                })
                .expect("retire unbound history");
        }
        let preserve = vec!["ACM010L9".to_string()];
        let allow_erase = vec!["CLN010L9".to_string(), "ACM011L9".to_string()];
        let report = StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &preserve,
            &allow_erase,
        )
        .expect("all-kinds preflight");
        assert_eq!(report.tapes.len(), 4);
        assert!(report
            .tapes
            .iter()
            .any(|tape| tape.tape_uuid == cleaning_uuid && tape.kind == "cleaning"));
        assert!(report.tapes.iter().any(|tape| {
            tape.tape_uuid == retired_uuid && tape.state == "retired" && tape.voltag.is_none()
        }));
        assert_eq!(report.paths.config.configured_path, config_path);
        assert_eq!(
            report.paths.config.canonical_path,
            fs::canonicalize(&config_path).expect("canonical config")
        );
        assert!(!report.paths.config.configured_path_is_symlink);

        let sqlite_path = temp.path().join("index/rem-state.sqlite");
        let conn = rusqlite::Connection::open(&sqlite_path).expect("open sqlite");
        conn.execute(
            "update tapes set voltag = null where tape_uuid = ?1",
            rusqlite::params![erase_uuid.as_slice()],
        )
        .expect("make active row unbound");
        drop(conn);
        StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &preserve,
            &allow_erase,
        )
        .expect_err("only unbound retired history may sit outside allowlists");
        let conn = rusqlite::Connection::open(&sqlite_path).expect("reopen sqlite");
        conn.execute(
            "update tapes set voltag = 'ACM011L9' where tape_uuid = ?1",
            rusqlite::params![erase_uuid.as_slice()],
        )
        .expect("restore active binding");
        drop(conn);

        // Advisory preflight does not authorize a later changed catalog. The
        // mutating API repeats admission under its own exclusive lock.
        {
            let mut handle =
                StateHandle::open_from_config_file(&config_path).expect("reopen state");
            handle
                .catalog_index()
                .provision_tape(crate::index::ProvisionTapeInput {
                    tape_uuid: [0xA4; 16],
                    voltag: "ACM013L9".to_string(),
                    block_size: 1024 * 1024,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("add admission drift");
        }
        fs::write(temp.path().join("audit/sentinel"), b"unchanged").expect("write sentinel");
        StateHandle::reset_catalog_preserving_with_allowlist_from_config_file(
            &config_path,
            &preserve,
            &allow_erase,
        )
        .expect_err("new bound tape must fail repeated admission");
        assert_eq!(
            fs::read(temp.path().join("audit/sentinel")).expect("sentinel survives"),
            b"unchanged"
        );
        assert!(!catalog_reset_fence_path(&StatePaths::from_config(
            &config_path,
            &load_config(&config_path).expect("config")
        ))
        .exists());
    }

    #[test]
    fn exact_full_erase_admits_schema_v16_but_preserve_and_drift_fail_closed() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-reset-v16-full-erase")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        seed_clean_break_reset_catalog(
            temp.path(),
            16,
            &[([0xC0; 16], "ACM020L9"), ([0xC1; 16], "ACM021L9")],
        );
        fs::write(temp.path().join("audit/sentinel"), b"unchanged").expect("write audit sentinel");

        let erase = vec![
            "ACM020L9".to_string(),
            "ACM021L9".to_string(),
            "ACM022L9".to_string(),
        ];
        let admitted =
            StateHandle::preflight_catalog_reset_from_config_file(&config_path, &[], &erase)
                .expect("schema-16 full erase preflight");
        assert_eq!(admitted.source_schema_version, 16);
        assert_eq!(admitted.tapes.len(), 2);
        assert!(admitted.preserve_tape_voltags.is_empty());

        let preserve_error = StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &["ACM020L9".to_string()],
            &["ACM021L9".to_string()],
        )
        .expect_err("legacy preservation must remain closed");
        assert!(
            preserve_error.to_string().contains("empty preserve list"),
            "{preserve_error}"
        );
        let incomplete_error = StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &[],
            &["ACM020L9".to_string()],
        )
        .expect_err("legacy erase must enumerate every bound tape");
        assert!(
            incomplete_error
                .to_string()
                .contains("outside the exact preserve and allow-erase"),
            "{incomplete_error}"
        );

        let conn = rusqlite::Connection::open(temp.path().join("index/rem-state.sqlite"))
            .expect("open legacy catalog for drift");
        conn.execute(
            "insert into tapes(
               tape_uuid, voltag, pool_id, kind, block_size, state, updated_at_utc
             ) values(?1, 'ACM022L9', 'camera.copy-a', 'data', 1048576, 'ready',
                      '2026-08-09T00:00:00Z')",
            rusqlite::params![[0xC2_u8; 16].as_slice()],
        )
        .expect("insert admitted-selector source drift");
        drop(conn);
        let stale = StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
            &config_path,
            &[],
            &erase,
            &admitted.preflight_token,
        )
        .expect_err("legacy source drift must invalidate preflight token");
        assert!(stale.to_string().contains("source changed after preflight"));
        assert_eq!(
            fs::read(temp.path().join("audit/sentinel")).expect("sentinel survives refusal"),
            b"unchanged"
        );

        let current =
            StateHandle::preflight_catalog_reset_from_config_file(&config_path, &[], &erase)
                .expect("refresh legacy full erase evidence");
        let reset = StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
            &config_path,
            &[],
            &erase,
            &current.preflight_token,
        )
        .expect("reset exact schema-16 source");
        assert!(reset.preserved_tapes.is_empty());

        let empty =
            StateHandle::preflight_catalog_reset_from_config_file(&config_path, &[], &erase)
                .expect("preflight current empty replacement");
        assert_eq!(empty.source_schema_version, crate::index::SCHEMA_VERSION);
        assert!(empty.tapes.is_empty());
        assert!(!empty.resume_exact);
    }

    #[test]
    fn exact_full_erase_admits_schema_v17() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-reset-v17-full-erase")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        seed_clean_break_reset_catalog(temp.path(), 17, &[([0xD0; 16], "ACM030L9")]);
        let erase = ["ACM030L9".to_string()];
        let admitted =
            StateHandle::preflight_catalog_reset_from_config_file(&config_path, &[], &erase)
                .expect("schema-17 full erase preflight");
        assert_eq!(admitted.source_schema_version, 17);
        StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
            &config_path,
            &[],
            &erase,
            &admitted.preflight_token,
        )
        .expect("reset exact schema-17 source");
    }

    #[test]
    fn reset_fence_binds_config_contents_and_rejects_changed_resume() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-reset-fence-config")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        let original_config = config_text(temp.path());
        fs::write(&config_path, &original_config).expect("write config");
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .catalog_index()
                .provision_tape(crate::index::ProvisionTapeInput {
                    tape_uuid: [0xA5; 16],
                    voltag: "ACM014L9".to_string(),
                    block_size: 1024 * 1024,
                    parity: ParityConfig::None,
                    force: false,
                })
                .expect("provision tape");
        }
        let config = load_config(&config_path).expect("load config");
        let admitted = StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &["ACM014L9".to_string()],
            &[],
        )
        .expect("preflight config fixture");
        let paths = StatePaths::from_config(&config_path, &config);
        let config_bytes = fs::read(&config_path).expect("config bytes");
        let digest = catalog_reset_request_digest(
            &paths,
            &config_bytes,
            &["ACM014L9".to_string()],
            &[],
            false,
            &admitted.preflight_token,
        )
        .expect("request digest");
        let lock = StateLockGuard::acquire(&paths.state_dir).expect("reset lock");
        let preserved = CatalogIndex::open_read_only(&paths.sqlite_path)
            .expect("source")
            .capture_catalog_reset_tapes(&["ACM014L9".to_string()], &config.tape_pool_rules)
            .expect("capture");
        let output_token =
            catalog_reset_intended_output_token(&admitted.tapes, &preserved).expect("output token");
        reset_catalog_locked_with_hook(
            &paths,
            &config,
            &preserved,
            CatalogResetAdmission {
                request_digest: &digest,
                preflight_token: &admitted.preflight_token,
                output_token: &output_token,
                source_schema_version: Some(admitted.source_schema_version),
            },
            |phase| {
                if phase == CatalogResetPhase::SourceCheckpointed {
                    Err(StateError::ConfigInvalid("injected config cut".to_string()))
                } else {
                    Ok(())
                }
            },
        )
        .expect_err("interrupt reset");
        drop(lock);

        fs::write(
            &config_path,
            format!("{original_config}\n# changed bytes\n"),
        )
        .expect("change config bytes");
        let changed_preflight = StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &["ACM014L9".to_string()],
            &[],
        )
        .expect_err("changed config bytes must not preflight as an exact resume");
        assert!(matches!(
            changed_preflight,
            StateError::CatalogResetInProgress(_)
        ));
        let changed = StateHandle::reset_catalog_preserving_from_config_file(
            &config_path,
            &["ACM014L9".to_string()],
        )
        .expect_err("same path with changed config must not resume");
        assert!(matches!(changed, StateError::CatalogResetInProgress(_)));
        assert!(catalog_reset_fence_path(&paths).exists());

        fs::write(&config_path, original_config).expect("restore exact config bytes");
        let exact_preflight = StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &["ACM014L9".to_string()],
            &[],
        )
        .expect("exact config bytes admit resume");
        assert!(exact_preflight.resume_exact);
        assert_eq!(exact_preflight.request_digest, digest);
        StateHandle::reset_catalog_preserving_from_config_file(
            &config_path,
            &["ACM014L9".to_string()],
        )
        .expect("exact request resumes");
        assert!(!catalog_reset_fence_path(&paths).exists());
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
                    .list_tapes(Some("camera.copy-a"), TapeKindFilter::Data)
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
        let tapes = handle
            .catalog_index()
            .list_tapes(None, TapeKindFilter::Data)
            .expect("list tapes");
        assert_eq!(tapes.len(), 1);
        assert_eq!(tapes[0].pool_id, None);
        assert!(handle
            .catalog_index()
            .list_tapes(Some("camera.copy-a"), TapeKindFilter::Data)
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
        let tapes = handle
            .catalog_index()
            .list_tapes(None, TapeKindFilter::Data)
            .expect("list tapes");
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
    fn audit_replay_projects_retire_and_provision_events_without_disturbing_sessions() {
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

        // Replay projects both the catalog lifecycle and the independent
        // operation/session history from the same ordered ledger.
        let mut restarted =
            StateHandle::open_with_config(paths, config).expect("open restarted handle");
        let report = restarted.startup_replay().expect("startup replay");

        assert_eq!(report.rebuild.audit_records_replayed, 4);
        assert_eq!(report.lost_operations_marked, 0);
        assert_eq!(report.lost_sessions_marked, 0);
        assert_eq!(
            restarted
                .catalog_index()
                .get_tape(&[0x21; 16])
                .expect("tape lookup")
                .expect("audit-projected tape")
                .state,
            "retired"
        );
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

    // -----------------------------------------------------------------
    //  Wrap-map cache and calibration lifecycle (design §§4.3, 6.5)
    // -----------------------------------------------------------------

    fn wrap_map_record(
        tape_uuid: [u8; 16],
        write_epoch: u64,
        generation: u64,
    ) -> WrapMapCacheRecord {
        WrapMapCacheRecord {
            tape_uuid,
            descriptors: vec![
                StoredWrapDescriptor {
                    partition: 0,
                    wrap_number: 0,
                    end_loi: 207_516,
                },
                StoredWrapDescriptor {
                    partition: 0,
                    wrap_number: 1,
                    end_loi: 415_522,
                },
                StoredWrapDescriptor {
                    partition: 0,
                    wrap_number: 2,
                    end_loi: 500_000,
                },
            ],
            mapped_extent_lba: 500_000,
            write_epoch,
            calibration_generation: generation,
            harvested_at_utc: "2026-08-04T00:00:00Z".to_string(),
        }
    }

    /// The cache stores raw descriptors and `mapped_extent_lba`
    /// separately, round-trips them exactly, and eviction of a single
    /// row works. No wrap start ever enters storage.
    #[test]
    fn wrap_map_cache_round_trips_raw_descriptors() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-wrapmap")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
        let tape_uuid = [0x42u8; 16];

        let record = wrap_map_record(tape_uuid, 3, 17);
        handle
            .catalog_index()
            .upsert_wrap_map(&record)
            .expect("upsert wrap map");
        let fetched = handle
            .catalog_index()
            .get_wrap_map(&tape_uuid)
            .expect("get wrap map")
            .expect("row present");
        assert_eq!(fetched, record, "raw descriptors round-trip unchanged");

        // Replacement at the next load harvest overwrites in place.
        let extended = wrap_map_record(tape_uuid, 4, 21);
        handle
            .catalog_index()
            .upsert_wrap_map(&extended)
            .expect("replace wrap map");
        let fetched = handle
            .catalog_index()
            .get_wrap_map(&tape_uuid)
            .expect("get wrap map")
            .expect("row present");
        assert_eq!(fetched.write_epoch, 4);

        assert!(handle
            .catalog_index()
            .delete_wrap_map(&tape_uuid)
            .expect("delete"));
        assert!(handle
            .catalog_index()
            .get_wrap_map(&tape_uuid)
            .expect("get after delete")
            .is_none());
        assert!(
            !handle
                .catalog_index()
                .delete_wrap_map(&tape_uuid)
                .expect("second delete"),
            "eviction is idempotent"
        );
    }

    #[test]
    fn state_handle_reprovision_evicts_old_identity_calibration() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-reprovision-calibration")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
        let old_uuid = [0x45; 16];
        let new_uuid = [0x46; 16];
        handle
            .provision_tape(ProvisionTapeInput {
                tape_uuid: old_uuid,
                voltag: "ACM045L9".to_string(),
                block_size: 1024 * 1024,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision old identity");
        let transition = handle
            .calibration_control()
            .record_harvest_success(old_uuid, 0)
            .expect("calibrate old identity");
        let crate::calibration::HarvestTransition::Calibrated {
            write_epoch,
            calibration_generation,
        } = transition
        else {
            panic!("expected calibrated transition: {transition:?}");
        };
        handle
            .catalog_index()
            .upsert_wrap_map(&wrap_map_record(
                old_uuid,
                write_epoch,
                calibration_generation,
            ))
            .expect("store old identity wrap map");

        handle
            .provision_tape(ProvisionTapeInput {
                tape_uuid: old_uuid,
                voltag: "ACM045L9".to_string(),
                block_size: 512 * 1024,
                parity: ParityConfig::None,
                force: true,
            })
            .expect("reprovision geometry under the same identity");
        let after_geometry_change = handle.calibration_control().row(old_uuid);
        assert_eq!(
            after_geometry_change.state,
            crate::calibration::VolumeCalibrationState::Uncalibrated
        );
        assert!(after_geometry_change.calibration_generation > calibration_generation);
        assert!(handle
            .catalog_index()
            .get_wrap_map(&old_uuid)
            .expect("map lookup after geometry change")
            .is_none());

        let recalibrated = handle
            .calibration_control()
            .record_harvest_success(old_uuid, write_epoch)
            .expect("recalibrate old identity");
        let crate::calibration::HarvestTransition::Calibrated {
            calibration_generation: recalibrated_generation,
            ..
        } = recalibrated
        else {
            panic!("expected recalibrated transition: {recalibrated:?}");
        };
        handle
            .catalog_index()
            .upsert_wrap_map(&wrap_map_record(
                old_uuid,
                write_epoch,
                recalibrated_generation,
            ))
            .expect("store replacement wrap map");

        handle
            .provision_tape(ProvisionTapeInput {
                tape_uuid: new_uuid,
                voltag: "ACM045L9".to_string(),
                block_size: 512 * 1024,
                parity: ParityConfig::None,
                force: true,
            })
            .expect("reprovision physical identity");

        assert!(handle
            .catalog_index()
            .get_wrap_map(&old_uuid)
            .expect("old wrap map lookup")
            .is_none());
        let old_control = handle.calibration_control().row(old_uuid);
        assert_eq!(
            old_control.state,
            crate::calibration::VolumeCalibrationState::Uncalibrated
        );
        assert!(old_control.calibration_generation > recalibrated_generation);
        assert!(!handle
            .calibration_control()
            .is_map_servable(old_uuid, write_epoch));
    }

    /// §6.5 "catalog projection rebuild" row: rebuild evicts the map
    /// table, the control rows and allocator remain, and every volume
    /// is uncalibrated under a fresh generation.
    #[test]
    fn projection_rebuild_evicts_wrap_maps_and_uncalibrates() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-rebuild-maps")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
        let tape_uuid = [0x43u8; 16];

        let transition = handle
            .calibration_control()
            .record_harvest_success(tape_uuid, 0)
            .expect("calibrate");
        let crate::calibration::HarvestTransition::Calibrated {
            write_epoch,
            calibration_generation,
        } = transition
        else {
            panic!("expected calibration, got {transition:?}");
        };
        handle
            .catalog_index()
            .upsert_wrap_map(&wrap_map_record(
                tape_uuid,
                write_epoch,
                calibration_generation,
            ))
            .expect("store map");
        assert!(handle
            .calibration_control()
            .is_map_servable(tape_uuid, write_epoch));

        handle.rebuild_index_from_journals().expect("rebuild");

        assert!(
            handle
                .catalog_index()
                .get_wrap_map(&tape_uuid)
                .expect("get after rebuild")
                .is_none(),
            "rebuild evicts the wrap-map projection rather than preserving it"
        );
        let row = handle.calibration_control().row(tape_uuid);
        assert_eq!(
            row.state,
            crate::calibration::VolumeCalibrationState::Uncalibrated,
            "the volume is uncalibrated until its next load harvest"
        );
        assert_eq!(row.write_epoch, write_epoch, "the epoch survives rebuild");
        assert!(
            row.calibration_generation > calibration_generation,
            "eviction stamped a fresh generation"
        );
        assert!(!handle
            .calibration_control()
            .is_map_servable(tape_uuid, write_epoch));
    }

    /// §6.5 "catalog reset" row plus the §4.3 monotonicity claim: the
    /// reset evicts every map and uncalibrates every volume, while the
    /// calibration-control store — including the generation allocator —
    /// survives in `state_dir/calibration/`, so no generation issued
    /// before the reset is ever issued again after it.
    #[test]
    fn catalog_reset_evicts_maps_and_never_reissues_generations() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-reset-maps")
            .tempdir()
            .expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let tape_uuid = [0x44u8; 16];

        let (pre_reset_generation, map_epoch) = {
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            let transition = handle
                .calibration_control()
                .record_harvest_success(tape_uuid, 0)
                .expect("calibrate");
            let crate::calibration::HarvestTransition::Calibrated {
                write_epoch,
                calibration_generation,
            } = transition
            else {
                panic!("expected calibration, got {transition:?}");
            };
            handle
                .catalog_index()
                .upsert_wrap_map(&wrap_map_record(
                    tape_uuid,
                    write_epoch,
                    calibration_generation,
                ))
                .expect("store map");
            (calibration_generation, write_epoch)
        };

        StateHandle::reset_catalog_from_config_file(&config_path).expect("reset catalog");

        // The calibration store's journal survived the reset in place.
        assert!(temp
            .path()
            .join("calibration")
            .join(crate::calibration::CALIBRATION_CONTROL_FILENAME)
            .is_file());

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen");
        assert!(
            handle
                .catalog_index()
                .get_wrap_map(&tape_uuid)
                .expect("get after reset")
                .is_none(),
            "reset evicts the wrap-map projection"
        );
        let row = handle.calibration_control().row(tape_uuid);
        assert_eq!(
            row.state,
            crate::calibration::VolumeCalibrationState::Uncalibrated
        );
        assert!(
            row.calibration_generation > pre_reset_generation,
            "the reset's eviction transition allocated a fresh generation"
        );
        assert!(!handle
            .calibration_control()
            .is_map_servable(tape_uuid, map_epoch));

        // The allocator never dips below its pre-reset high-water
        // mark: the very next generation issued for ANY volume is
        // strictly greater than everything issued before the reset.
        let fresh = handle
            .calibration_control()
            .record_harvest_failure([0x55u8; 16])
            .expect("post-reset transition");
        assert!(
            fresh > pre_reset_generation,
            "a generation issued before the reset is never reissued after it"
        );
    }

    /// §6.5 startup/orphan-recovery row: a write session that was
    /// non-terminal when the process died leaves its volume invalid —
    /// epoch durably advanced, uncalibrated — until a fresh load
    /// harvest. A lost read session does not.
    #[test]
    fn startup_replay_invalidates_possibly_written_volumes() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-state-startup-cal")
            .tempdir()
            .expect("temp dir");
        let config = parse_config_toml(&config_text(temp.path())).expect("config");
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let write_tape = [0x66u8; 16];
        let read_tape = [0x77u8; 16];
        let write_session = Uuid::from_u128(0x71);
        let read_session = Uuid::from_u128(0x72);

        let (write_map_epoch, read_map_epoch) = {
            let mut handle =
                StateHandle::open_with_config(paths.clone(), config.clone()).expect("open");
            for (session_id, kind, tape) in [
                (write_session, "write", write_tape),
                (read_session, "read", read_tape),
            ] {
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
                            kind: kind.to_string(),
                            id: Some(session_id.to_string()),
                        },
                        detail: BTreeMap::from([
                            (
                                "session_kind".to_string(),
                                CborValue::Text(kind.to_string()),
                            ),
                            ("tape_uuid".to_string(), CborValue::Bytes(tape.to_vec())),
                        ]),
                    })
                    .expect("append session opened");
            }
            let mut epochs = Vec::new();
            for tape in [write_tape, read_tape] {
                let transition = handle
                    .calibration_control()
                    .record_harvest_success(tape, 0)
                    .expect("calibrate");
                let crate::calibration::HarvestTransition::Calibrated { write_epoch, .. } =
                    transition
                else {
                    panic!("expected calibration");
                };
                epochs.push(write_epoch);
            }
            (epochs[0], epochs[1])
        };

        // The process "dies" (handle dropped) and a new one replays.
        let mut handle = StateHandle::open_with_config(paths, config).expect("reopen");
        let report = handle.startup_replay().expect("startup replay");
        assert_eq!(report.lost_sessions_marked, 2);

        let write_row = handle.calibration_control().row(write_tape);
        assert_eq!(
            write_row.write_epoch,
            write_map_epoch + 1,
            "possibly-written volume's epoch durably advanced"
        );
        assert_eq!(
            write_row.state,
            crate::calibration::VolumeCalibrationState::Uncalibrated
        );
        assert!(
            !handle
                .calibration_control()
                .is_map_servable(write_tape, write_map_epoch),
            "invalid until a fresh load harvest"
        );

        let read_row = handle.calibration_control().row(read_tape);
        assert_eq!(
            read_row.write_epoch, read_map_epoch,
            "a lost read session dispatches nothing media-modifying and is not invalidated"
        );
    }
}
