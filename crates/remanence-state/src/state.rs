//! Public Layer 4 state handle.

mod authority_reset;

use crate::authority::TapeAuthorityView;
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use remanence_parity::ParityScheme;
use sha2::{Digest, Sha256};
#[cfg(test)]
use time::Duration;

use crate::audit::{AuditActor, FileAuditLog, SourceLayer};
#[cfg(test)]
use crate::audit::{AuditEvent, AuditEventRecord, AuditSink, AuditSubject};
use crate::calibration::CalibrationControlStore;
use crate::config::{
    derive_tape_pool_from_voltag, load_config, parse_config_toml, validate_trusted_volume_paths,
    RemConfig,
};
use crate::error::StateError;
use crate::index::{
    AdoptBootstrapIdentityInput, AdoptBootstrapIdentityOutcome, AdoptedTapeState,
    AuditReplayReport, CatalogIndex, ProvisionTapeInput, RebuildReport, RetireTapeInput,
    RetireTapeOutcome, TapeIoFenceRecord, TapeJournalIndexReport, TapeRecord,
};
use crate::lock::StateLockGuard;
use crate::paths::StatePaths;

/// Open Layer 4 state owner.
#[derive(Debug)]
pub struct StateHandle {
    paths: StatePaths,
    config: RemConfig,
    config_warnings: Vec<StateConfigWarning>,
    _lock: std::sync::Arc<StateLockGuard>,
    library: crate::library_runtime::LibraryRuntime,
    catalog: crate::ProjectionClient,
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
    /// Durable identity of the exact endpoint used for the physical recheck.
    pub endpoint: crate::DriveKey,
    /// Changer inquiry revision, required for a Bay and absent for Standalone.
    pub library_revision: Option<String>,
    /// Return storage element, required for a Bay and absent for Standalone.
    pub home_slot: Option<u16>,
    /// Inquiry serial of the selected drive.
    pub drive_serial: String,
    /// Exact Bootstrap hardware-compression flag (must be false).
    pub bootstrap_drive_compression: bool,
    /// Verified drive configuration used for the physical read (must be false).
    pub configured_drive_compression: bool,
    /// Typed observed post-Bootstrap layout.
    pub physical_tail: BootstrapAdoptionTailEvidence,
}

/// One validation funnel for live adoption and authoritative audit replay.
pub(crate) fn validate_bootstrap_endpoint_evidence(
    endpoint: &crate::DriveKey,
    library_revision: Option<&str>,
    home_slot: Option<u16>,
    drive_serial: &str,
) -> Result<(), StateError> {
    let valid_text = |value: &str| !value.is_empty() && value.trim() == value;
    let valid = valid_text(drive_serial)
        && match endpoint {
            crate::DriveKey::Bay { library_serial, .. } => {
                valid_text(library_serial)
                    && library_revision.is_some_and(valid_text)
                    && home_slot.is_some()
            }
            crate::DriveKey::Standalone {
                drive_serial: serial,
            } => serial == drive_serial && library_revision.is_none() && home_slot.is_none(),
        };
    if !valid {
        return Err(StateError::ConfigInvalid(
            "bootstrap adoption has invalid endpoint evidence".into(),
        ));
    }
    Ok(())
}

/// Adoption records validated on-tape geometry, including parity-off, for
/// both library and standalone endpoints. The caller checks drive limits.
pub(crate) fn validate_bootstrap_adoption_geometry(
    _endpoint: &crate::DriveKey,
    block_size: u32,
    parity: &remanence_parity::ParityConfig,
) -> Result<(), StateError> {
    if block_size == 0 || !block_size.is_multiple_of(512) {
        return Err(StateError::TapeProvisionConflict(
            "invalid bootstrap block size".into(),
        ));
    }
    if let remanence_parity::ParityConfig::Scheme(scheme) = parity {
        scheme
            .validate()
            .map_err(|e| StateError::TapeProvisionConflict(e.to_string()))?;
    }
    Ok(())
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
    /// Compatibility field, always zero: reset never opens SQL as inventory.
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
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogResetPreflightTape {
    /// Exact tape UUID.
    pub tape_uuid: [u8; 16],
    /// Bound volume tag, absent for unbound retired history.
    pub voltag: Option<String>,
    /// Label provenance preserved across reset.
    pub voltag_source: Option<String>,
    /// Recorded media generation preserved across reset.
    pub cartridge_generation: Option<String>,
    /// Generation provenance preserved across reset.
    pub generation_source: Option<String>,
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
        authority_reset::preflight(
            config_path.as_ref(),
            preserve_tape_voltags,
            allow_erase_tape_voltags,
        )
    }

    /// Open state with already-resolved paths and a parsed config.
    pub fn open_with_config(paths: StatePaths, config: RemConfig) -> Result<Self, StateError> {
        crate::authority_manifest::AuthorityManifest::preflight(&paths)?;
        let lock = std::sync::Arc::new(StateLockGuard::acquire(&paths.state_dir)?);
        ensure_no_catalog_reset_fence(&paths)?;
        ensure_state_directories(&paths)?;
        crate::authority_layout::admit(&paths)?;
        create_private_dir(&paths.journal_dir.join("tapes"))?;
        validate_trusted_volume_paths(&config)?;
        let library =
            crate::library_runtime::LibraryRuntime::open(&paths, config.audit.fsync, lock.clone())
                .map_err(|e| StateError::ConfigInvalid(e.to_string()))?;
        let mut accepted = authority_reset::configuration_record(&config)?;
        let prior_configuration = library.configuration();
        if let Some(prior) = &prior_configuration {
            accepted.generation = prior.generation;
        }
        if prior_configuration.as_ref() != Some(&accepted) {
            accepted.generation = prior_configuration.map_or(Ok(1), |v| {
                v.generation.checked_add(1).ok_or_else(|| {
                    StateError::ConfigInvalid("configuration generation exhausted".into())
                })
            })?;
            library
                .append(|a, view| {
                    a.configuration_accepted(
                        accepted,
                        AuditActor::System,
                        SourceLayer::Layer4,
                        view,
                    )
                })
                .map_err(|e| StateError::ConfigInvalid(e.to_string()))?;
        }
        let catalog =
            crate::projector::open(&paths.sqlite_path)?.with_library_runtime(library.clone());
        let index = catalog.write_handle();
        let config_warnings = configured_tape_pool_warnings(&config);
        let calibration = CalibrationControlStore::open(&paths.calibration_dir)?;
        Ok(Self {
            paths,
            config,
            config_warnings,
            _lock: lock,
            library,
            catalog,
            index,
            calibration,
        })
    }

    /// Reset local catalog state with already-resolved paths and parsed config.
    pub fn reset_catalog_with_config(
        paths: StatePaths,
        config: RemConfig,
    ) -> Result<(), StateError> {
        authority_reset::execute(paths, config, &[], &[], None, true).map(|_| ())
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
        authority_reset::execute(
            paths,
            config,
            preserve_tape_voltags,
            allow_erase_tape_voltags,
            expected_preflight_token,
            false,
        )
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

    /// Compatibility catch-up after an external typed producer completes.
    pub fn catch_up_authority_projection(&mut self) -> Result<(), StateError> {
        self.library
            .catch_up()
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))
    }

    /// Read the catalog through the state's existing handle.
    pub fn catalog(&self) -> &CatalogIndex {
        &self.index
    }

    /// Issue a writer from the state's projector owner.
    pub fn write_handle(&self) -> CatalogIndex {
        self.catalog.write_handle()
    }

    /// Share the projector owner with standalone drive actors.
    pub fn projection_client(&self) -> crate::ProjectionClient {
        self.catalog.clone()
    }

    /// Share the one manifest-bound library authority with actors and operators.
    pub fn library_runtime(&self) -> crate::library_runtime::LibraryRuntime {
        self.library.clone()
    }

    /// Provision or reprovision one tape while coordinating any old physical
    /// identity's durable wrap-map calibration eviction.
    pub fn provision_tape(&mut self, input: ProvisionTapeInput) -> Result<(), StateError> {
        let observation = crate::MediaObservation::label_hint(&input.voltag);
        self.provision_tape_with_media(input, crate::LabelProvenance::Changer, observation)
    }

    /// Provision observed media through the calibration-owning state boundary.
    pub fn provision_tape_with_media(
        &mut self,
        input: ProvisionTapeInput,
        provenance: crate::LabelProvenance,
        observation: Option<crate::MediaObservation>,
    ) -> Result<(), StateError> {
        self.provision_replacing_media(input, provenance, observation, None)
    }

    /// Confirmed BOT replacement shares provisioning and calibration eviction.
    pub fn provision_replacing_media(
        &mut self,
        input: ProvisionTapeInput,
        provenance: crate::LabelProvenance,
        observation: Option<crate::MediaObservation>,
        previous_uuid: Option<[u8; 16]>,
    ) -> Result<(), StateError> {
        let prior = match self
            .index
            .get_tape(&previous_uuid.unwrap_or(input.tape_uuid))?
        {
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
        use crate::library_log as lib;
        let observation = observation.ok_or_else(|| StateError::TapeProvisionConflict(
            "Reprovision requires recorded medium family; provisioning without generation awaits a representation ruling".into()))?;
        let medium = lib::MediumSpec {
            family: lib::MediumFamily::Lto(observation.generation),
            usable_logical_bytes: observation.generation.raw_capacity_bytes(),
            record_size: input.block_size,
            rerecord: true,
            lifecycle_ops: true,
            source: observation.source,
        };
        // A replacing producer must retire its predecessor explicitly. Do not
        // manufacture an identity from the old SQL row to make this append pass.
        let predecessor = previous_uuid
            .map(|uuid| {
                self.library
                    .tape_state(uuid)
                    .filter(|t| t.retired)
                    .map(|t| t.tape)
                    .ok_or_else(|| {
                        StateError::TapeProvisionConflict(
                            "Reprovision predecessor must be retired in library authority".into(),
                        )
                    })
            })
            .transpose()?;
        let pool = derive_tape_pool_from_voltag(&input.voltag, &self.config.tape_pool_rules)
            .map(Into::into);
        let parity_scheme = match input.parity {
            remanence_parity::ParityConfig::None => None,
            remanence_parity::ParityConfig::Scheme(scheme) => Some(lib::ParityScheme {
                id: scheme.id.as_str().into(),
                data_blocks_per_stripe: scheme.data_blocks_per_stripe,
                parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
                stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            }),
        };
        self.library
            .append(|authority, view| {
                authority.reprovision(
                    lib::Reprovision {
                        tape: lib::TapeRef {
                            uuid: input.tape_uuid,
                            incarnation: 1,
                        },
                        predecessor,
                        mode: lib::ProvisionMode::Initialize,
                        label: Some(input.voltag.as_str().into()),
                        label_provenance: Some(match provenance {
                            crate::LabelProvenance::Changer => lib::LabelProvenance::Changer,
                            crate::LabelProvenance::Operator => lib::LabelProvenance::Operator,
                        }),
                        parity_scheme,
                        medium,
                        pool,
                        endpoint: None,
                        adoption: None,
                    },
                    AuditActor::local_user(),
                    SourceLayer::Layer4,
                    view,
                )
            })
            .map_err(|e| StateError::TapeProvisionConflict(e.to_string()))?;
        if let Some(prior_uuid) = prior_uuid_to_evict {
            self.calibration.record_map_evicted(prior_uuid)?;
        }
        Ok(())
    }

    /// Assign or unassign under native admission and typed generation checks.
    /// The library record is durable before projection; replay completes an
    /// interrupted projection. A refused setter never appends authority.
    pub fn assign_tape_pool(
        &mut self,
        tape_uuid: [u8; 16],
        expected_generation: u64,
        pool_id: Option<&str>,
    ) -> Result<crate::TapeAssignmentSnapshot, StateError> {
        use crate::library_log as lib;
        let tape = self.library.tape_state(tape_uuid).ok_or_else(|| {
            StateError::TapePoolAssignmentConflict("tape has no Reprovision authority".into())
        })?;
        if pool_id.is_some_and(|id| !self.config.tape_pools.iter().any(|p| p.id == id)) {
            return Err(StateError::TapePoolAssignmentConflict(
                "pool is not configured".into(),
            ));
        }
        let authority = crate::TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            tape_uuid,
            &self.paths.sqlite_path,
        )?;
        let owner = authority.acquire_exclusive()?;
        self.library
            .append_identity(&[&owner], |a, v, native| {
                a.assign_pool(
                    lib::AssignPool {
                        tape: tape.tape,
                        pool: pool_id.map(Into::into),
                        change: lib::GenerationChange {
                            expected: expected_generation,
                            next: 0,
                        },
                        source: lib::AssignmentSource::Operator,
                        configuration: None,
                    },
                    expected_generation,
                    native,
                    AuditActor::local_user(),
                    SourceLayer::Layer4,
                    v,
                )
            })
            .map_err(|e| StateError::TapePoolAssignmentConflict(e.to_string()))?;
        let current = self
            .library
            .tape_state(tape_uuid)
            .ok_or_else(|| StateError::IndexCorrupt("assigned identity disappeared".into()))?;
        Ok(crate::TapeAssignmentSnapshot {
            tape_uuid: tape_uuid.to_vec(),
            pool_id: current.pool.map(|v| v.as_str().into()),
            assignment_generation: current.assignment_generation,
        })
    }

    /// Record a pool projection already established by provisioning/rules using
    /// the same event schema as later operator assignments, including nulls.
    pub fn record_rule_tape_pool_assignment(
        &mut self,
        tape_uuid: [u8; 16],
    ) -> Result<(), StateError> {
        let tape = self.library.tape_state(tape_uuid).ok_or_else(|| {
            StateError::TapePoolAssignmentConflict("tape has no Reprovision authority".into())
        })?;
        let expected = tape.label.as_ref().and_then(|label| {
            derive_tape_pool_from_voltag(label.as_str(), &self.config.tape_pool_rules)
        });
        if tape.pool.as_ref().map(|pool| pool.as_str()) != expected {
            return Err(StateError::TapePoolAssignmentConflict(
                "rule assignment changed; publish explicit AssignPool under current configuration"
                    .into(),
            ));
        }
        // Initial rule assignment is already part of Reprovision's atomic
        // identity snapshot. Never import a later SQL row as another authority.
        Ok(())
    }

    /// Persist a BOT-proven cartridge's correction before projecting its cache.
    /// Only generation/provenance change; provisioning and pool facts are untouched.
    pub fn set_tape_generation(
        &mut self,
        tape_uuid: [u8; 16],
        observation: crate::MediaObservation,
    ) -> Result<(), StateError> {
        use crate::library_log as lib;
        let tape = self.library.tape_state(tape_uuid).ok_or_else(|| {
            StateError::TapeProvisionConflict("tape has no Reprovision authority".into())
        })?;
        let mut medium = tape.medium.clone();
        medium.family = lib::MediumFamily::Lto(observation.generation);
        medium.usable_logical_bytes = observation.generation.raw_capacity_bytes();
        medium.source = observation.source;
        if medium == tape.medium {
            return Ok(());
        }
        self.library
            .append(|authority, view| {
                authority.medium_spec_set(
                    lib::MediumSpecSet {
                        tape: tape.tape,
                        medium,
                        change: lib::GenerationChange {
                            expected: tape.media_revision,
                            next: tape.media_revision.checked_add(1).ok_or({
                                lib::AuthorityError::Store(lib::store::StoreError::Fold(
                                    lib::FoldError::Sequence,
                                ))
                            })?,
                        },
                    },
                    tape.media_revision,
                    observation.source,
                    AuditActor::local_user(),
                    SourceLayer::Layer4,
                    view,
                )
            })
            .map_err(|e| StateError::TapeProvisionConflict(e.to_string()))?;
        Ok(())
    }

    /// Rebind a shell label after the caller proves the target BOT identity.
    /// Audit is fsynced before projection, so replay completes an interrupted rebind.
    pub fn rebind_tape_label(
        &mut self,
        tape_uuid: [u8; 16],
        label: &str,
        previous_uuid: [u8; 16],
    ) -> Result<(), StateError> {
        use crate::library_log as lib;
        let tape = self.library.tape_state(tape_uuid).ok_or_else(|| {
            StateError::TapeProvisionConflict("target has no Reprovision authority".into())
        })?;
        let prior = self.library.tape_state(previous_uuid).ok_or_else(|| {
            StateError::TapeProvisionConflict("prior holder has no Reprovision authority".into())
        })?;
        if tape_uuid == previous_uuid || prior.label.as_ref().map(|v| v.as_str()) != Some(label) {
            return Err(StateError::TapeProvisionConflict(
                "label prior holder differs from BOT-proven correction".into(),
            ));
        }
        let target_authority = crate::TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            tape_uuid,
            &self.paths.sqlite_path,
        )?;
        let prior_authority = crate::TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            previous_uuid,
            &self.paths.sqlite_path,
        )?;
        let target_owner = target_authority.acquire_exclusive()?;
        let prior_owner = prior_authority.acquire_exclusive()?;
        self.library
            .append_identity(&[&target_owner, &prior_owner], |a, v, native| {
                a.label(
                    lib::Label {
                        tape: tape.tape,
                        label: Some(label.into()),
                        provenance: lib::LabelProvenance::Operator,
                        change: lib::GenerationChange {
                            expected: tape.label_generation,
                            next: 0,
                        },
                        prior_holder: Some(lib::PriorHolder {
                            tape: prior.tape,
                            change: lib::GenerationChange {
                                expected: prior.label_generation,
                                next: prior.label_generation.checked_add(1).ok_or({
                                    lib::AuthorityError::Store(lib::store::StoreError::Fold(
                                        lib::FoldError::GenerationMismatch,
                                    ))
                                })?,
                            },
                        }),
                        observed_bot_uuid: Some(tape_uuid),
                    },
                    tape.label_generation,
                    native,
                    AuditActor::local_user(),
                    SourceLayer::Layer4,
                    v,
                )
            })
            .map_err(|e| StateError::TapeProvisionConflict(e.to_string()))?;
        Ok(())
    }

    /// Durably adopt a checksum-valid BOT Bootstrap as identity-only state.
    ///
    /// Authenticated Reprovision and current native/library state prove an exact
    /// identity-only retry, including fresh operation UUIDs. Changed immutable
    /// facts or evolved authority refuse. The shared writer syncs the source
    /// before projection; retries rebuild derived SQL without importing it.
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
        validate_bootstrap_adoption_geometry(&evidence.endpoint, input.block_size, &input.parity)?;
        if evidence.bootstrap_drive_compression || evidence.configured_drive_compression {
            return Err(StateError::TapeProvisionConflict(
                "bootstrap adoption requires drive compression disabled".into(),
            ));
        }
        validate_bootstrap_endpoint_evidence(
            &evidence.endpoint,
            evidence.library_revision.as_deref(),
            evidence.home_slot,
            &evidence.drive_serial,
        )?;
        let state = evidence.physical_tail.adopted_state();
        let pool_id = if let Some(pool) = &input.pool_id {
            if pool.trim().is_empty() || pool.trim() != pool {
                return Err(StateError::ConfigInvalid(
                    "adoption pool must be non-empty and trimmed".into(),
                ));
            }
            pool.clone()
        } else {
            input
                .voltag
                .as_deref()
                .and_then(|label| derive_tape_pool_from_voltag(label, &self.config.tape_pool_rules))
                .ok_or_else(|| {
                    StateError::TapeProvisionConflict(format!(
                        "bootstrap barcode {} does not match any locked tape_pool_rule",
                        input.voltag.as_deref().unwrap_or("unlabelled")
                    ))
                })?
                .to_string()
        };
        if !self.config.tape_pools.iter().any(|pool| pool.id == pool_id) {
            return Err(StateError::TapeProvisionConflict(format!(
                "bootstrap barcode {} derives unconfigured pool {pool_id}",
                input.voltag.as_deref().unwrap_or("unlabelled")
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
        use crate::library_log as lib;
        let observation = input.observation.ok_or_else(|| StateError::TapeProvisionConflict(
            "Reprovision adoption requires recorded medium family; unknown-generation adoption awaits a representation ruling".into()))?;
        let parity_scheme = match &input.parity {
            remanence_parity::ParityConfig::None => None,
            remanence_parity::ParityConfig::Scheme(scheme) => Some(lib::ParityScheme {
                id: scheme.id.as_str().into(),
                data_blocks_per_stripe: scheme.data_blocks_per_stripe,
                parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
                stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            }),
        };
        let physical_tail = match evidence.physical_tail {
            BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod => {
                lib::PhysicalTail::ExactBootstrapFilemarkEod
            }
            BootstrapAdoptionTailEvidence::DataAfterBootstrap => {
                lib::PhysicalTail::DataAfterBootstrap
            }
            BootstrapAdoptionTailEvidence::MissingFilemark => lib::PhysicalTail::MissingFilemark,
            BootstrapAdoptionTailEvidence::ExtraFilemark => lib::PhysicalTail::ExtraFilemark,
            BootstrapAdoptionTailEvidence::Ambiguous => lib::PhysicalTail::Ambiguous,
        };
        if self.calibration.row(input.tape_uuid).state
            == crate::calibration::VolumeCalibrationState::Calibrated
        {
            return Err(StateError::TapeProvisionConflict(
                "Bootstrap identity has a calibrated wrap map".into(),
            ));
        }
        let newly_adopted = self
            .library
            .adopt_identity(
                lib::Reprovision {
                    tape: lib::TapeRef {
                        uuid: input.tape_uuid,
                        incarnation: 1,
                    },
                    predecessor: None,
                    mode: lib::ProvisionMode::Adopt,
                    label: input.voltag.as_deref().map(Into::into),
                    label_provenance: input.voltag.as_ref().map(|_| match input.label_provenance {
                        crate::LabelProvenance::Changer => lib::LabelProvenance::Changer,
                        crate::LabelProvenance::Operator => lib::LabelProvenance::Operator,
                    }),
                    parity_scheme,
                    medium: lib::MediumSpec {
                        family: lib::MediumFamily::Lto(observation.generation),
                        usable_logical_bytes: observation.generation.raw_capacity_bytes(),
                        record_size: input.block_size,
                        rerecord: true,
                        lifecycle_ops: true,
                        source: observation.source,
                    },
                    pool: Some(pool_id.as_str().into()),
                    endpoint: Some(match &evidence.endpoint {
                        crate::DriveKey::Bay {
                            library_serial,
                            bay,
                        } => lib::DriveKey::Bay(lib::Bay {
                            library_serial: library_serial.as_str().into(),
                            bay: *bay,
                        }),
                        crate::DriveKey::Standalone { drive_serial } => {
                            lib::DriveKey::Standalone(lib::Standalone {
                                drive_serial: drive_serial.as_str().into(),
                            })
                        }
                    }),
                    adoption: Some(lib::Adoption {
                        request_fingerprint,
                        operation: *input.operation_id.as_bytes(),
                        idempotency_key: *input.operation_id.as_bytes(),
                        physical_tail,
                        state: match state {
                            AdoptedTapeState::Ready => lib::AdoptedState::Ready,
                            AdoptedTapeState::RecoveryRequired => {
                                lib::AdoptedState::RecoveryRequired
                            }
                        },
                        library_revision: evidence.library_revision.as_deref().map(Into::into),
                        home_slot: evidence.home_slot,
                        drive_serial: evidence.drive_serial.as_str().into(),
                        identity_only: true,
                        bootstrap_drive_compression: evidence.bootstrap_drive_compression,
                        configured_drive_compression: evidence.configured_drive_compression,
                    }),
                },
                |original| {
                    let Some(prior) = &original.adoption else {
                        return false;
                    };
                    let Some(endpoint) = &original.endpoint else {
                        return false;
                    };
                    // A new physical check may observe another placement or tail with
                    // the same adopted state. Reconstruct the original ceremony with
                    // the current identity/pool intent; its authenticated fingerprint
                    // proves that intent without importing a SQL assignment source.
                    let original_evidence = BootstrapAdoptionEvidence {
                        endpoint: match endpoint {
                            lib::DriveKey::Bay(v) => {
                                crate::DriveKey::new(v.library_serial.as_str(), v.bay)
                            }
                            lib::DriveKey::Standalone(v) => crate::DriveKey::Standalone {
                                drive_serial: v.drive_serial.as_str().into(),
                            },
                        },
                        library_revision: prior
                            .library_revision
                            .as_ref()
                            .map(|v| v.as_str().into()),
                        home_slot: prior.home_slot,
                        drive_serial: prior.drive_serial.as_str().into(),
                        bootstrap_drive_compression: prior.bootstrap_drive_compression,
                        configured_drive_compression: prior.configured_drive_compression,
                        physical_tail: match prior.physical_tail {
                            lib::PhysicalTail::ExactBootstrapFilemarkEod => {
                                BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod
                            }
                            lib::PhysicalTail::DataAfterBootstrap => {
                                BootstrapAdoptionTailEvidence::DataAfterBootstrap
                            }
                            lib::PhysicalTail::MissingFilemark => {
                                BootstrapAdoptionTailEvidence::MissingFilemark
                            }
                            lib::PhysicalTail::ExtraFilemark => {
                                BootstrapAdoptionTailEvidence::ExtraFilemark
                            }
                            lib::PhysicalTail::Ambiguous => {
                                BootstrapAdoptionTailEvidence::Ambiguous
                            }
                        },
                    };
                    let Some(source) = adoption_assignment_source(original) else {
                        return false;
                    };
                    if input.pool_id.is_some() && source != "operator" {
                        return false;
                    }
                    let mut normalized = input.clone();
                    normalized.pool_id = (source == "operator").then(|| pool_id.clone());
                    original_evidence.physical_tail.adopted_state() == state
                        && bootstrap_adoption_request_fingerprint(
                            &normalized,
                            &original_evidence,
                            &pool_id,
                            state,
                            &geometry,
                        ) == prior.request_fingerprint
                },
            )
            .map_err(|e| StateError::TapeProvisionConflict(e.to_string()))?;
        Ok(AdoptBootstrapIdentityOutcome {
            newly_adopted,
            assignment_generation: 1,
            state,
            operation_id: input.operation_id,
            request_fingerprint,
        })
    }

    /// Return the durable calibration-control store (cloneable
    /// handle). This is the authority on wrap-map servability; it
    /// survives projection rebuild and catalog reset.
    pub fn calibration_control(&self) -> &CalibrationControlStore {
        &self.calibration
    }

    /// Replay the authoritative audit log into SQLite-derived projections.
    pub fn replay_audit_projection(&mut self) -> Result<AuditReplayReport, StateError> {
        self.library
            .catch_up()
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        self.index.typed_replay_report()
    }

    /// Compatibility name for the projection-only audit and tape-log rebuild.
    pub fn rebuild_index_from_journals(&mut self) -> Result<RebuildReport, StateError> {
        self.rebuild_projection_from_tape_logs()
    }

    /// Offline projection-only entry point. Unlike opening a normal StateHandle,
    /// this never opens an audit writer (which could rotate or trim a segment).
    /// State ownership covers validation, snapshots, SQL and calibration eviction.
    pub fn rebuild_projection_from_config_file(
        config_path: impl AsRef<Path>,
    ) -> Result<RebuildReport, StateError> {
        let config_path = config_path.as_ref();
        let config = crate::load_config(config_path)?;
        let paths = StatePaths::from_config(config_path, &config);
        crate::authority_manifest::AuthorityManifest::preflight(&paths)?;
        let _owner = StateLockGuard::acquire(&paths.state_dir)?;
        ensure_no_catalog_reset_fence(&paths)?;
        crate::authority_layout::validate_owned_config_layout(&config)?;
        if !paths.state_dir.join("authority-format").exists() {
            return Err(StateError::ConfigInvalid(
                "projection rebuild requires an existing authority-format marker".into(),
            ));
        }
        let catalog = crate::projector::open(&paths.sqlite_path)?;
        let mut index = catalog.write_handle();
        let calibration = CalibrationControlStore::open(&paths.calibration_dir)?;
        Self::rebuild_projection_inputs(&paths, &mut index, &calibration)
    }

    /// Rebuild derived SQLite rows without changing tape or audit logs. Evict
    /// wrap maps and invalidate their calibration using the existing allocator.
    pub fn rebuild_projection_from_tape_logs(&mut self) -> Result<RebuildReport, StateError> {
        crate::authority_layout::validate_owned_config_layout(&self.config)?;
        let report = self
            .library
            .rebuild_projection(&mut self.index)
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        self.calibration
            .record_all_maps_evicted("projection_rebuild")?;
        Ok(report)
    }

    /// Only immutable authority inputs and derived stores enter this operation.
    /// It has neither an audit writer nor access to any reset helper.
    fn rebuild_projection_inputs(
        paths: &StatePaths,
        index: &mut CatalogIndex,
        calibration: &CalibrationControlStore,
    ) -> Result<RebuildReport, StateError> {
        use crate::library_log as lib;
        let manifest = crate::authority_manifest::AuthorityManifest::inspect_for_reset(paths)?
            .ok_or_else(|| {
                StateError::ConfigInvalid(
                    "typed rebuild requires an authority manifest; run declared reset".into(),
                )
            })?;
        let view =
            lib::native_view::NativeTapeView::probe(manifest.tape_root(), &paths.sqlite_path)
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        lib::session_ack::validate(&view, |applied| {
            lib::store::replay_observed(
                manifest.library_root(),
                manifest.library_uuid(),
                &view,
                applied,
            )
        })?;
        let natives = view
            .snapshots()
            .map(crate::tape_log::authority::AuthoritySnapshot::new)
            .collect::<Result<Vec<_>, _>>()?;
        let report = index
            .rebuild_typed_sources(
                |applied| {
                    lib::store::replay_observed(
                        manifest.library_root(),
                        manifest.library_uuid(),
                        &view,
                        applied,
                    )
                },
                &natives,
            )
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        calibration.record_all_maps_evicted("projection_rebuild")?;
        Ok(report)
    }

    /// Explicit offline tail repair. A cut below the projection mark requires
    /// rebuilding this tape in the same fenced operation; media is never touched.
    pub fn repair_tape_log_tail(
        &mut self,
        uuid: [u8; 16],
        through: u64,
        rebuild_projection: bool,
    ) -> Result<crate::tape_log::store::RepairEvidence, StateError> {
        crate::authority_layout::admit(&self.paths)?;
        self.library
            .repair_native(
                &mut self.index,
                crate::library_log::TapeRef {
                    uuid,
                    incarnation: 1,
                },
                through,
                rebuild_projection,
            )
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))
    }

    /// Run startup replay and mark non-terminal prior work as lost by restart.
    pub fn startup_replay(&mut self) -> Result<StartupReplayReport, StateError> {
        let rebuild = self.rebuild_index_from_journals()?;
        let lost_operations_marked = self.mark_lost_operations_by_restart()?;
        let lost_sessions_marked = self.recover_sessions_after_restart(None, |_, _| Ok(()))?;
        Ok(StartupReplayReport {
            rebuild,
            lost_operations_marked,
            lost_sessions_marked,
        })
    }

    /// Retire through typed library authority while the native lease proves
    /// closeout and finalization admission. Projection follows durable source.
    pub fn retire_tape(&mut self, input: RetireTapeInput) -> Result<RetireTapeOutcome, StateError> {
        use crate::library_log as lib;
        if input.reason.trim().is_empty() {
            return Err(StateError::TapeProvisionConflict(
                "retirement reason is empty".into(),
            ));
        }
        let tape = self.library.tape_state(input.tape_uuid).ok_or_else(|| {
            StateError::TapeProvisionConflict("tape has no Reprovision authority".into())
        })?;
        if tape.retired {
            return Ok(RetireTapeOutcome {
                newly_retired: false,
                released_voltag: None,
                instances_marked_missing: 0,
            });
        }
        let authority = crate::TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            input.tape_uuid,
            &self.paths.sqlite_path,
        )?;
        authority.check_finalizing()?;
        let lease = authority.acquire_exclusive()?;
        let snapshot = lease.freeze()?;
        if let Some(refusal) = snapshot.tape_summary().closeout_required("retirement") {
            return Err(StateError::TapeProvisionConflict(refusal));
        }
        self.library
            .refresh_native(&snapshot)
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        self.library
            .append_identity(&[&lease], |a, v, native| {
                a.retire(
                    lib::Retire {
                        tape: tape.tape,
                        reason: lib::RetireReason::OperatorRequested,
                        observed_tape_sequence: (snapshot.log_reference().0 != 0)
                            .then_some(snapshot.log_reference().0),
                        assignment_generation: tape.assignment_generation,
                        detail: Some(input.reason.as_str().into()),
                    },
                    tape.assignment_generation,
                    native,
                    AuditActor::local_user(),
                    SourceLayer::Layer4,
                    v,
                )
            })
            .map_err(|e| StateError::TapeProvisionConflict(e.to_string()))?;
        self.calibration.record_map_evicted(input.tape_uuid)?;
        Ok(RetireTapeOutcome {
            newly_retired: true,
            released_voltag: tape.label.map(|v| v.as_str().into()),
            instances_marked_missing: snapshot.raw.summary().object_count,
        })
    }

    /// Sync release evidence before clearing a tape-I/O fence under the state lock.
    pub fn release_tape_io_fence(
        &mut self,
        quarantine_id: &str,
        ack: &str,
    ) -> Result<Option<TapeIoFenceRecord>, StateError> {
        self.library
            .release_tape_fence(quarantine_id, ack, AuditActor::local_user())
            .map_err(|e| StateError::ConfigInvalid(e.to_string()))
    }

    /// Replay one Layer 3c journal through the 3c shared reader and index it.
    pub fn ingest_tape_journal(
        &mut self,
        tape_uuid: [u8; 16],
        block_size: u32,
        scheme: ParityScheme,
    ) -> Result<TapeJournalIngestionOutcome, StateError> {
        let authority = crate::TapeAuthority::open(
            self.paths.journal_dir.join("tapes"),
            tape_uuid,
            &self.paths.sqlite_path,
        )?;
        let snapshot = match authority.freeze() {
            Ok(snapshot) => snapshot,
            Err(StateError::TapeLog(error))
                if matches!(*error, crate::tape_log::store::StoreError::Locked(_)) =>
            {
                return Ok(TapeJournalIngestionOutcome::Pending(
                    self.index
                        .mark_tape_journal_ingestion_pending(tape_uuid, block_size, &scheme)?,
                ));
            }
            Err(error) => return Err(error),
        };
        use crate::authority::TapeAuthoritySnapshot;
        if snapshot.summary().block_size != Some(block_size)
            || snapshot.raw.header().scheme.as_ref() != Some(&scheme)
        {
            return Err(StateError::JournalReplayFailed(
                "tape-log ingestion geometry mismatch".into(),
            ));
        }
        self.index.project_tape_log(&snapshot)?;
        Ok(TapeJournalIngestionOutcome::Indexed(
            TapeJournalIndexReport {
                ingestion_pending: false,
                tape_files_rebuilt: snapshot.summary().next_file,
                object_instances_rebuilt: snapshot.summary().object_count,
            },
        ))
    }

    fn mark_lost_operations_by_restart(&mut self) -> Result<u64, StateError> {
        self.library
            .mark_operations_lost()
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))
    }

    /// Recover prior sessions under an owned state/endpoint boundary. A scoped
    /// caller must hold the durable endpoint lock. The callback persists any
    /// additional quarantine before a session can be marked terminal.
    pub fn recover_sessions_after_restart(
        &mut self,
        endpoint: Option<&crate::DriveKey>,
        mut quarantine: impl FnMut(&mut Self, &crate::RestartSession) -> Result<(), StateError>,
    ) -> Result<u64, StateError> {
        use crate::library_log as lib;
        let sessions = self
            .library
            .open_sessions()
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        let mut count = 0;
        for (mut source, reference) in sessions {
            let session = crate::library_runtime::restart::session_projection(&source);
            if endpoint.is_some_and(|key| session.drive_key.as_ref() != Some(key)) {
                continue;
            }
            quarantine(self, &session)?;
            source.previous = Some(reference);
            source.event = lib::SessionEvent::LostByRestart;
            source.reason = Some("daemon_restart".into());
            self.library
                .append(|a, v| {
                    a.session_observed(source, AuditActor::System, SourceLayer::Layer4, v)
                })
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
            count += 1;
        }
        // A native write session has no endpoint or restart-ending vocabulary.
        // Preserve its unresolved source; never manufacture a read-session row
        // or a classified fence from broad native end reasons (C8-G2).
        if endpoint.is_none() {
            for session in self.library.open_native_sessions() {
                invalidate_restart_session_volume(&self.calibration, &session)?;
                quarantine(self, &session)?;
            }
        }
        Ok(count)
    }
}

/// Replay audit projections, then mark prior sessions lost on configured library bays.
///
/// Call before actors or requests. The caller's state-directory lock is the only
/// protection against sweeping a live process's session. Step 5 (device-lock-only
/// CLI writers) must replace it with a recorded owner identity or a lock proof.
/// Standalone endpoints, other libraries, and sessions without an endpoint are
/// not marked lost. This performs no device I/O, fencing, or checkpoint recovery.
pub fn recover_library_sessions_after_restart(
    index: &mut CatalogIndex,
    config: &RemConfig,
) -> Result<u64, StateError> {
    use crate::library_log as lib;
    let runtime = index.library_runtime().ok_or_else(|| {
        StateError::ConfigInvalid(
            "startup session recovery requires the shared library owner".into(),
        )
    })?;
    runtime
        .catch_up()
        .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
    let mut count = 0;
    for (mut source, reference) in runtime
        .open_sessions()
        .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?
    {
        let Some(lib::DriveKey::Bay(endpoint)) = &source.endpoint else {
            continue;
        };
        if !config
            .libraries
            .iter()
            .any(|v| v.serial.trim() == endpoint.library_serial.as_str())
        {
            continue;
        }
        source.previous = Some(reference);
        source.event = lib::SessionEvent::LostByRestart;
        source.reason = Some("daemon_restart".into());
        runtime
            .append(|a, v| a.session_observed(source, AuditActor::System, SourceLayer::Layer4, v))
            .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?;
        count += 1;
    }
    Ok(count)
}

/// Preserve the existing conservative invalidation for possibly written volumes.
fn invalidate_restart_session_volume(
    calibration: &CalibrationControlStore,
    session: &crate::RestartSession,
) -> Result<(), StateError> {
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
            calibration.record_possible_write_recovery(tape_uuid)?;
        }
    }
    Ok(())
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
    field(
        &mut hash,
        "voltag",
        input.voltag.as_deref().unwrap_or_default().as_bytes(),
    );
    if input.label_provenance != crate::LabelProvenance::Changer
        || input.observation
            != input
                .voltag
                .as_deref()
                .and_then(crate::MediaObservation::label_hint)
        || input.pool_id.is_some()
    {
        field(
            &mut hash,
            "voltag_source",
            input.label_provenance.as_str().as_bytes(),
        );
        field(
            &mut hash,
            "assignment_source",
            if input.pool_id.is_some() {
                b"operator"
            } else {
                b"rule"
            },
        );
        if let Some(observation) = input.observation {
            field(
                &mut hash,
                "cartridge_generation",
                observation.generation.media_code().as_bytes(),
            );
            field(
                &mut hash,
                "generation_source",
                observation.source.as_str().as_bytes(),
            );
        }
    }

    field(&mut hash, "block_size", &input.block_size.to_be_bytes());
    field(&mut hash, "geometry", geometry.as_bytes());
    field(&mut hash, "pool_id", pool_id.as_bytes());
    field(&mut hash, "state", state.as_str().as_bytes());
    match &evidence.endpoint {
        crate::DriveKey::Bay {
            library_serial,
            bay,
        } => {
            // Preserve historical Bay fingerprints exactly.
            field(&mut hash, "library_serial", library_serial.as_bytes());
            field(
                &mut hash,
                "library_revision",
                evidence
                    .library_revision
                    .as_deref()
                    .unwrap_or_default()
                    .as_bytes(),
            );
            field(
                &mut hash,
                "home_slot",
                &evidence.home_slot.unwrap_or_default().to_be_bytes(),
            );
            field(&mut hash, "drive_element", &bay.to_be_bytes());
        }
        crate::DriveKey::Standalone { .. } => field(
            &mut hash,
            "endpoint_key",
            evidence.endpoint.encode().as_bytes(),
        ),
    }
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

/// Recover legacy pool-intent provenance only when the authenticated adoption
/// fingerprint proves one of the original explicit producer inputs. Arbitrary
/// external fingerprints remain unknown; no SQL or configuration is consulted.
pub(crate) fn adoption_assignment_source(
    value: &crate::library_log::Reprovision,
) -> Option<&'static str> {
    use crate::library_log as lib;
    let adoption = value.adoption.as_ref()?;
    let endpoint = value.endpoint.as_ref()?;
    let pool = value.pool.as_ref()?.as_str();
    let lib::MediumFamily::Lto(generation) = value.medium.family else {
        return None;
    };
    let physical_tail = match adoption.physical_tail {
        lib::PhysicalTail::ExactBootstrapFilemarkEod => {
            BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod
        }
        lib::PhysicalTail::DataAfterBootstrap => BootstrapAdoptionTailEvidence::DataAfterBootstrap,
        lib::PhysicalTail::MissingFilemark => BootstrapAdoptionTailEvidence::MissingFilemark,
        lib::PhysicalTail::ExtraFilemark => BootstrapAdoptionTailEvidence::ExtraFilemark,
        lib::PhysicalTail::Ambiguous => BootstrapAdoptionTailEvidence::Ambiguous,
    };
    let evidence = BootstrapAdoptionEvidence {
        endpoint: match endpoint {
            lib::DriveKey::Bay(v) => crate::DriveKey::new(v.library_serial.as_str(), v.bay),
            lib::DriveKey::Standalone(v) => crate::DriveKey::Standalone {
                drive_serial: v.drive_serial.as_str().into(),
            },
        },
        library_revision: adoption
            .library_revision
            .as_ref()
            .map(|v| v.as_str().into()),
        home_slot: adoption.home_slot,
        drive_serial: adoption.drive_serial.as_str().into(),
        bootstrap_drive_compression: adoption.bootstrap_drive_compression,
        configured_drive_compression: adoption.configured_drive_compression,
        physical_tail,
    };
    let (parity, geometry) = match &value.parity_scheme {
        None => (
            remanence_parity::ParityConfig::None,
            "no-parity".to_string(),
        ),
        Some(v) => (
            remanence_parity::ParityConfig::Scheme(remanence_parity::ParityScheme {
                id: remanence_parity::SchemeId::new_owned(v.id.as_str().into()),
                data_blocks_per_stripe: v.data_blocks_per_stripe,
                parity_blocks_per_stripe: v.parity_blocks_per_stripe,
                stripes_per_neighborhood: v.stripes_per_neighborhood,
            }),
            format!(
                "scheme={} data={} parity={} stripes={}",
                v.id.as_str(),
                v.data_blocks_per_stripe,
                v.parity_blocks_per_stripe,
                v.stripes_per_neighborhood
            ),
        ),
    };
    let provenances = match value.label_provenance {
        Some(lib::LabelProvenance::Changer) => vec![crate::LabelProvenance::Changer],
        Some(lib::LabelProvenance::Operator) => vec![crate::LabelProvenance::Operator],
        None => vec![
            crate::LabelProvenance::Changer,
            crate::LabelProvenance::Operator,
        ],
    };
    for provenance in provenances {
        for source in ["rule", "operator"] {
            let input = AdoptBootstrapIdentityInput {
                operation_id: uuid::Uuid::from_bytes(adoption.operation),
                tape_uuid: value.tape.uuid,
                voltag: value.label.as_ref().map(|v| v.as_str().into()),
                block_size: value.medium.record_size,
                parity: parity.clone(),
                label_provenance: provenance,
                observation: Some(crate::MediaObservation {
                    generation,
                    source: value.medium.source,
                }),
                pool_id: (source == "operator").then(|| pool.into()),
            };
            if bootstrap_adoption_request_fingerprint(
                &input,
                &evidence,
                pool,
                physical_tail.adopted_state(),
                &geometry,
            ) == adoption.request_fingerprint
            {
                return Some(source);
            }
        }
    }
    None
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

fn hash_catalog_reset_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn catalog_reset_fence_path(paths: &StatePaths) -> PathBuf {
    paths.state_dir.join(CATALOG_RESET_FENCE_FILE)
}

pub(crate) fn ensure_no_catalog_reset_fence(paths: &StatePaths) -> Result<(), StateError> {
    ensure_no_catalog_reset_at(&paths.state_dir)
}

/// Bare-lock motion recovery shares startup's reset exclusion without opening SQL.
/// Treat every fence entry, including a dangling symlink, as unresolved; failed
/// metadata reads cannot establish that a reset is absent.
pub(crate) fn ensure_no_catalog_reset_at(state_dir: &Path) -> Result<(), StateError> {
    let fence = state_dir.join(CATALOG_RESET_FENCE_FILE);
    match fs::symlink_metadata(&fence) {
        Ok(_) => Err(StateError::CatalogResetInProgress(format!(
            "durable fence {} requires an exact reset rerun",
            fence.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(StateError::io_at(
            "inspect catalog reset fence",
            &fence,
            error,
        )),
    }
}

pub(crate) fn catalog_reset_fence_evidence(
    paths: &StatePaths,
) -> Result<Option<(String, String, String)>, StateError> {
    let fence = catalog_reset_fence_path(paths);
    match fs::symlink_metadata(&fence) {
        Ok(metadata) if metadata.is_file() => (),
        Ok(_) => {
            return Err(StateError::CatalogResetInProgress(
                "reset fence is not a regular file".into(),
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(StateError::io_at("inspect reset fence", &fence, error)),
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

#[cfg(any(test, feature = "fixtures"))]
thread_local! { static REJECT_RESET_HELPER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }

/// Test-only guard that makes every destructive reset route fail immediately.
#[cfg(any(test, feature = "fixtures"))]
pub struct RejectResetHelpers(bool);
#[cfg(any(test, feature = "fixtures"))]
impl RejectResetHelpers {
    /// Install on the current thread, including CLI entry-point tests.
    pub fn install() -> Self {
        Self(REJECT_RESET_HELPER.replace(true))
    }
}
#[cfg(any(test, feature = "fixtures"))]
impl Drop for RejectResetHelpers {
    fn drop(&mut self) {
        REJECT_RESET_HELPER.set(self.0);
    }
}

fn reset_helper_tripwire() {
    #[cfg(any(test, feature = "fixtures"))]
    REJECT_RESET_HELPER
        .with(|flag| assert!(!flag.get(), "projection rebuild reached reset helper"));
}

#[cfg(test)]
fn hex_tape_uuid(tape_uuid: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in tape_uuid {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("write to string");
    }
    out
}

fn configured_tape_pool_warnings(config: &RemConfig) -> Vec<StateConfigWarning> {
    let mut warnings = Vec::new();
    if config.tape_pool_rules.is_empty() && !config.tape_pools.is_empty() {
        warnings.push(StateConfigWarning::TapePoolsWithoutRules {
            pool_count: config.tape_pools.len(),
        });
    }
    warnings
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

const RESET_ARCHIVE_DIR: &str = "reset-archives";

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

    #[cfg(test)]
    use ciborium::value::Value as CborValue;
    use remanence_parity::ParityConfig;
    use uuid::Uuid;

    use super::*;
    use crate::config::parse_config_toml;
    use crate::index::{StoredWrapDescriptor, TapeKindFilter, WrapMapCacheRecord};

    pub(super) fn config_text(root: &Path) -> String {
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
            voltag: Some("ACM901L9".to_string()),
            label_provenance: crate::LabelProvenance::Changer,
            observation: crate::MediaObservation::label_hint("ACM901L9"),
            pool_id: None,
            block_size: 1024 * 1024,
            parity: ParityConfig::Scheme(remanence_parity::default_scheme_for_block_size(
                1024 * 1024,
            )),
        }
    }

    fn adoption_evidence(tail: BootstrapAdoptionTailEvidence) -> BootstrapAdoptionEvidence {
        BootstrapAdoptionEvidence {
            endpoint: crate::DriveKey::new("LIB001", 0x0101),
            library_revision: Some("D.00".to_string()),
            home_slot: Some(0x0401),
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

        let handle = StateHandle::open_from_config_file(&config_path).expect("open state");

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
            handle.catalog().schema_version().expect("schema version"),
            crate::index::SCHEMA_VERSION
        );
        assert_eq!(
            handle
                .catalog()
                .get_tape_pool("camera.copy-a")
                .expect("get configured pool")
                .expect("pool exists")
                .display_name
                .as_deref(),
            Some("Camera copy A")
        );
    }

    #[test]
    fn operator_adoption_records_media_and_explicit_pool_intent_for_replay() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
        let mut input = adoption_input(Uuid::new_v4());
        input.voltag = Some("handwritten".into());
        input.label_provenance = crate::LabelProvenance::Operator;
        input.observation = Some(crate::MediaObservation {
            generation: crate::LtoGen::Lto7,
            source: crate::GenerationSource::Measured,
        });
        input.pool_id = Some("camera.copy-a".into());
        handle
            .adopt_bootstrap_identity(
                input.clone(),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .unwrap();
        let records = FileAuditLog::replay(&handle.paths.audit_dir).unwrap();
        let mut restored = crate::projector::open(":memory:")
            .map(|catalog| catalog.write_handle())
            .unwrap();
        restored.replay_audit_records(&records).unwrap();
        for index in [&mut handle.index, &mut restored] {
            index
                .reconcile_tape_pool_projection_from_rules(&[], &[])
                .unwrap();
            let tape = index.get_tape(&input.tape_uuid).unwrap().unwrap();
            assert_eq!(tape.voltag_source, Some(crate::LabelProvenance::Operator));
            assert_eq!(
                crate::tape_generation(&tape),
                Some((crate::LtoGen::Lto7, crate::GenerationSource::Measured))
            );
            assert_eq!(tape.pool_id.as_deref(), Some("camera.copy-a"));
        }
        let mut changed = input.clone();
        changed.observation = crate::MediaObservation::label_hint("L9");
        assert!(handle
            .adopt_bootstrap_identity(
                changed,
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod)
            )
            .is_err());
    }

    /// Corrections are authority for adopted tapes too, and stale or failed
    /// assignment transactions must not append a replayable change.
    #[test]
    fn metadata_corrections_rebuild_adopted_identity_and_refused_assignment_is_not_audited() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
        let mut input = adoption_input(Uuid::new_v4());
        input.observation = Some(crate::MediaObservation {
            generation: crate::LtoGen::Lto7,
            source: crate::GenerationSource::Measured,
        });
        input.pool_id = Some("camera.copy-a".into());
        handle
            .adopt_bootstrap_identity(
                input.clone(),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .unwrap();
        let before = handle
            .catalog()
            .get_tape(&input.tape_uuid)
            .unwrap()
            .unwrap();
        let source_before = handle.library.test_source_records();
        assert!(handle
            .assign_tape_pool(input.tape_uuid, before.assignment_generation + 1, None)
            .is_err());
        assert_eq!(handle.library.test_source_records(), source_before);
        handle
            .assign_tape_pool(input.tape_uuid, before.assignment_generation, None)
            .unwrap();
        // Approved MediumSpecSet preserves an already measured family. An
        // operator cannot replace that fact or increase unmeasured capacity.
        for generation in [crate::LtoGen::Lto8, crate::LtoGen::Lto9] {
            let source_before = handle.library.test_source_records();
            assert!(handle
                .set_tape_generation(
                    input.tape_uuid,
                    crate::MediaObservation {
                        generation,
                        source: crate::GenerationSource::Operator
                    }
                )
                .is_err());
            assert_eq!(handle.library.test_source_records(), source_before);
        }
        handle
            .set_tape_generation(
                input.tape_uuid,
                crate::MediaObservation {
                    generation: crate::LtoGen::Lto7,
                    source: crate::GenerationSource::Operator,
                },
            )
            .unwrap();
        let mut unmeasured = adoption_input(Uuid::new_v4());
        unmeasured.tape_uuid = *Uuid::new_v4().as_bytes();
        unmeasured.voltag = Some("HINT01L7".into());
        unmeasured.pool_id = Some("camera.copy-a".into());
        unmeasured.observation = Some(crate::MediaObservation {
            generation: crate::LtoGen::Lto7,
            source: crate::GenerationSource::Label,
        });
        handle
            .adopt_bootstrap_identity(
                unmeasured.clone(),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .unwrap();
        let source_before = handle.library.test_source_records();
        assert!(handle
            .set_tape_generation(
                unmeasured.tape_uuid,
                crate::MediaObservation {
                    generation: crate::LtoGen::Lto8,
                    source: crate::GenerationSource::Operator
                }
            )
            .is_err());
        assert_eq!(handle.library.test_source_records(), source_before);
        handle
            .set_tape_generation(
                unmeasured.tape_uuid,
                crate::MediaObservation {
                    generation: crate::LtoGen::Lto8,
                    source: crate::GenerationSource::Measured,
                },
            )
            .unwrap();
        for rebuild in [false, true] {
            if rebuild {
                handle.rebuild_index_from_journals().unwrap();
            }
            let tape = handle
                .catalog()
                .get_tape(&input.tape_uuid)
                .unwrap()
                .unwrap();
            assert_eq!(
                crate::tape_generation(&tape),
                Some((crate::LtoGen::Lto7, crate::GenerationSource::Operator))
            );
            assert_eq!(tape.pool_id, None);
            assert_eq!(tape.assignment_generation, before.assignment_generation + 1);
            let corrected = handle
                .catalog()
                .get_tape(&unmeasured.tape_uuid)
                .unwrap()
                .unwrap();
            assert_eq!(
                crate::tape_generation(&corrected),
                Some((crate::LtoGen::Lto8, crate::GenerationSource::Measured))
            );
        }
    }

    #[test]
    fn standalone_adoption_without_label_preserves_typed_evidence_through_replay() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
        let mut input = adoption_input(Uuid::new_v4());
        input.voltag = None;
        input.label_provenance = crate::LabelProvenance::Operator;
        input.pool_id = Some("camera.copy-a".into());
        input.observation = Some(crate::MediaObservation {
            generation: crate::LtoGen::Lto7,
            source: crate::GenerationSource::Measured,
        });
        let evidence = BootstrapAdoptionEvidence {
            endpoint: crate::DriveKey::Standalone {
                drive_serial: "DRV001".into(),
            },
            library_revision: None,
            home_slot: None,
            ..adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod)
        };
        handle
            .adopt_bootstrap_identity(input.clone(), evidence.clone())
            .unwrap();
        assert!(
            !handle
                .adopt_bootstrap_identity(input.clone(), evidence.clone())
                .unwrap()
                .newly_adopted
        );
        let records = FileAuditLog::replay(&handle.paths.audit_dir).unwrap();
        let record = records
            .iter()
            .find(|r| r.event == AuditEvent::TapeIdentityAdopted)
            .unwrap();
        assert_eq!(record.detail.get("voltag"), Some(&CborValue::Null));
        assert_eq!(
            record.detail.get("endpoint_key"),
            Some(&CborValue::Text(evidence.endpoint.encode()))
        );
        for field in [
            "library_serial",
            "library_revision",
            "home_slot",
            "drive_element",
        ] {
            assert!(!record.detail.contains_key(field));
        }
        let mut restored = crate::projector::open(":memory:")
            .map(|catalog| catalog.write_handle())
            .unwrap();
        restored.replay_audit_records(&records).unwrap();
        restored
            .rebuild_from_authoritative_sources(&records, &[])
            .unwrap();
        let expected = handle.index.get_tape(&input.tape_uuid).unwrap().unwrap();
        let actual = restored.get_tape(&input.tape_uuid).unwrap().unwrap();
        assert_eq!(
            (actual.voltag.as_deref(), actual.voltag_source),
            (None, None)
        );
        assert_eq!(
            crate::tape_generation(&actual),
            crate::tape_generation(&expected)
        );
        assert_eq!(actual.pool_id, expected.pool_id);
        assert_eq!(actual.assignment_generation, expected.assignment_generation);
        // Typed endpoint and inquiry evidence must identify the same drive.
        let mut invalid = evidence.clone();
        invalid.drive_serial = "different".into();
        assert!(handle
            .adopt_bootstrap_identity(input.clone(), invalid)
            .is_err());
        let mut bad_records = records.clone();
        bad_records
            .iter_mut()
            .find(|r| r.event == AuditEvent::TapeIdentityAdopted)
            .unwrap()
            .detail
            .insert("drive_serial".into(), CborValue::Text("different".into()));
        let mut rejected = crate::projector::open(":memory:")
            .map(|catalog| catalog.write_handle())
            .unwrap();
        rejected.replay_audit_records(&bad_records).unwrap();
        assert!(rejected.get_tape(&input.tape_uuid).unwrap().is_none());
        // Bay admission still requires revision and return slot, live and replay.
        for missing_revision in [false, true] {
            let mut bay =
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod);
            if missing_revision {
                bay.library_revision = None;
            } else {
                bay.home_slot = None;
            }
            assert!(handle.adopt_bootstrap_identity(input.clone(), bay).is_err());
            let mut bad = record.clone();
            bad.detail.insert(
                "endpoint_key".into(),
                CborValue::Text(crate::DriveKey::new("LIB001", 0).encode()),
            );
            if missing_revision {
                bad.detail
                    .insert("home_slot".into(), CborValue::Integer(1.into()));
            } else {
                bad.detail
                    .insert("library_revision".into(), CborValue::Text("D.00".into()));
            }
            let mut rejected = crate::projector::open(":memory:")
                .map(|catalog| catalog.write_handle())
                .unwrap();
            rejected.replay_audit_records(&[bad]).unwrap();
            assert!(rejected.get_tape(&input.tape_uuid).unwrap().is_none());
        }
        drop(handle);
        let mut reopened = StateHandle::open_from_config_file(&config_path).unwrap();
        assert!(
            !reopened
                .adopt_bootstrap_identity(input, evidence)
                .unwrap()
                .newly_adopted
        );
    }

    #[test]
    fn media_aware_state_provisioning_retains_calibration_eviction() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
        let uuid = [0x45; 16];
        let mut input = ProvisionTapeInput {
            tape_uuid: uuid,
            voltag: "pen label".into(),
            block_size: 1024 * 1024,
            parity: ParityConfig::None,
            force: false,
        };
        let observation = crate::MediaObservation {
            generation: crate::LtoGen::Lto7,
            source: crate::GenerationSource::Measured,
        };
        handle
            .provision_tape_with_media(
                input.clone(),
                crate::LabelProvenance::Operator,
                Some(observation),
            )
            .unwrap();
        let crate::calibration::HarvestTransition::Calibrated {
            write_epoch,
            calibration_generation,
        } = handle
            .calibration_control()
            .record_harvest_success(uuid, 0)
            .unwrap()
        else {
            panic!("expected calibration");
        };
        handle
            .index
            .upsert_wrap_map(&wrap_map_record(uuid, write_epoch, calibration_generation))
            .unwrap();
        // An idempotent call retains calibration; geometry replacement evicts it.
        handle
            .provision_tape_with_media(
                input.clone(),
                crate::LabelProvenance::Operator,
                Some(observation),
            )
            .unwrap();
        assert!(handle
            .calibration_control()
            .is_map_servable(uuid, write_epoch));
        input.block_size = 512 * 1024;
        input.force = true;
        // Packet tag4 requires a new UUID/incarnation1. A refused same-UUID
        // geometry change must leave calibration and the cached map intact.
        assert!(handle
            .provision_tape_with_media(
                input.clone(),
                crate::LabelProvenance::Operator,
                Some(observation)
            )
            .is_err());
        assert!(handle
            .calibration_control()
            .is_map_servable(uuid, write_epoch));
        assert!(handle.index.get_wrap_map(&uuid).unwrap().is_some());
        handle
            .retire_tape(RetireTapeInput {
                tape_uuid: uuid,
                reason: "confirmed replacement".into(),
            })
            .unwrap();
        input.tape_uuid = [0x46; 16];
        handle
            .provision_replacing_media(
                input.clone(),
                crate::LabelProvenance::Operator,
                Some(observation),
                Some(uuid),
            )
            .unwrap();
        let tape = handle.index.get_tape(&input.tape_uuid).unwrap().unwrap();
        assert_eq!(tape.voltag_source, Some(crate::LabelProvenance::Operator));
        assert_eq!(
            crate::tape_generation(&tape),
            Some((observation.generation, observation.source))
        );
        assert!(handle.index.get_wrap_map(&uuid).unwrap().is_none());
        assert!(!handle
            .calibration_control()
            .is_map_servable(uuid, write_epoch));
        let control = handle.calibration_control().row(uuid);
        assert_eq!(
            control.state,
            crate::calibration::VolumeCalibrationState::Uncalibrated
        );
        assert!(control.calibration_generation > calibration_generation);
    }

    #[test]
    fn rebind_is_audited_and_survives_state_reopen_and_rebuild() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
        for (id, label) in [(1u8, "ACM001L9"), (2, "ACM002L9")] {
            crate::FileAuditLog::open(&handle.paths().audit_dir, true)
                .unwrap()
                .append(AuditEventRecord {
                    actor: AuditActor::System,
                    source_layer: SourceLayer::Layer4,
                    operation_id: None,
                    session_id: None,
                    idempotency_key: None,
                    event: AuditEvent::TapeProvisioned,
                    subject: AuditSubject {
                        kind: "tape".into(),
                        id: Some(hex_tape_uuid([id; 16])),
                    },
                    detail: BTreeMap::from([("voltag".into(), CborValue::Text(label.into()))]),
                })
                .unwrap();
        }
        handle.replay_audit_projection().unwrap();
        handle
            .rebind_tape_label([2; 16], "ACM001L9", [1; 16])
            .unwrap();
        let records = FileAuditLog::replay(&handle.paths.audit_dir).unwrap();
        assert!(records
            .iter()
            .any(|r| r.event == AuditEvent::TapeLabelRebound));
        drop(handle);
        let mut reopened = StateHandle::open_from_config_file(&config_path).unwrap();
        reopened
            .index
            .rebuild_from_authoritative_sources(&records, &[])
            .unwrap();
        assert_eq!(
            reopened.index.get_tape(&[1; 16]).unwrap().unwrap().voltag,
            None
        );
        assert_eq!(
            reopened
                .index
                .get_tape_by_voltag("ACM001L9")
                .unwrap()
                .unwrap()
                .tape_uuid,
            vec![2; 16]
        );
    }

    /// Persist recorded library geometry through live adoption, reopen and audit rebuild.
    #[test]
    fn bootstrap_adoption_preserves_recorded_library_geometry() {
        let mut custom = remanence_parity::default_scheme_for_block_size(512 * 1024);
        custom.stripes_per_neighborhood = 1;
        for (block_size, parity) in [
            (256 * 1024, ParityConfig::None),
            (
                256 * 1024,
                ParityConfig::Scheme(remanence_parity::default_scheme_for_block_size(256 * 1024)),
            ),
            (
                1024 * 1024,
                ParityConfig::Scheme(remanence_parity::default_scheme_for_block_size(1024 * 1024)),
            ),
            (512 * 1024, ParityConfig::Scheme(custom)),
        ] {
            let temp = tempfile::tempdir().expect("temp dir");
            let config_path = temp.path().join("config.toml");
            fs::write(&config_path, config_text(temp.path())).expect("write config");
            let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            let mut input = adoption_input(Uuid::new_v4());
            input.block_size = block_size;
            input.parity = parity;
            let outcome = handle
                .adopt_bootstrap_identity(
                    input.clone(),
                    adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
                )
                .expect("adopt recorded geometry");
            assert_eq!(outcome.state, AdoptedTapeState::Ready);
            let tape = handle.index.get_tape(&input.tape_uuid).unwrap().unwrap();
            assert_eq!(tape.block_size, Some(u64::from(block_size)));
            match &input.parity {
                ParityConfig::None => {
                    assert!(tape.scheme_id.is_none());
                    assert!(tape.data_blocks_per_stripe.is_none());
                    assert!(tape.parity_blocks_per_stripe.is_none());
                    assert!(tape.stripes_per_neighborhood.is_none());
                }
                ParityConfig::Scheme(scheme) => {
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
                }
            }
            let records = FileAuditLog::replay(&handle.paths.audit_dir).expect("read audit");
            drop(handle);
            let mut reopened = StateHandle::open_from_config_file(&config_path).expect("reopen");
            reopened
                .index
                .rebuild_from_authoritative_sources(&records, &[])
                .expect("rebuild adoption from audit");
            let rebuilt = reopened.index.get_tape(&input.tape_uuid).unwrap().unwrap();
            assert_eq!(rebuilt, tape);
        }
    }

    /// Both recorded and effective compression must stay off at the durable boundary.
    #[test]
    fn bootstrap_adoption_refuses_compression_with_unchanged_message() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).expect("write config");
        let mut handle = StateHandle::open_from_config_file(&config_path).expect("open state");
        for (recorded, configured) in [(true, false), (false, true), (true, true)] {
            let mut input = adoption_input(Uuid::new_v4());
            input.block_size = 256 * 1024;
            input.parity = ParityConfig::None;
            let mut evidence =
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod);
            evidence.bootstrap_drive_compression = recorded;
            evidence.configured_drive_compression = configured;
            let error = handle
                .adopt_bootstrap_identity(input.clone(), evidence)
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "tape provisioning conflict: bootstrap adoption requires drive compression disabled"
            );
            assert!(handle.index.get_tape(&input.tape_uuid).unwrap().is_none());
        }
        assert!(FileAuditLog::replay(&handle.paths.audit_dir)
            .unwrap()
            .is_empty());
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
        fresh_evidence.home_slot = Some(0x0402);
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
        assert_eq!(
            fs::read_dir(handle.paths().journal_dir.join("tapes"))
                .unwrap()
                .count(),
            0
        );
        let mut explicit_pool = adoption_input(Uuid::new_v4());
        explicit_pool.pool_id = Some("camera.copy-a".into());
        assert!(
            handle
                .adopt_bootstrap_identity(
                    explicit_pool,
                    adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod)
                )
                .is_err(),
            "a repeated rule adoption must not silently discard explicit operator intent"
        );

        let tape = handle
            .catalog()
            .get_tape(&adoption_input(operation_id).tape_uuid)
            .expect("lookup")
            .expect("adopted row");
        assert_eq!(tape.state, "ready");
        assert_eq!(tape.pool_id.as_deref(), Some("camera.copy-a"));
        assert_eq!(tape.total_committed_ordinals, 0);
        assert!(handle
            .catalog()
            .list_tape_files(&adoption_input(operation_id).tape_uuid)
            .expect("list tape files")
            .is_empty());
        let records = handle.library.test_source_records();
        let adoption_records: Vec<_> = records
            .iter()
            .filter(|record| {
                matches!(
                    record.body_view(),
                    crate::library_log::BodyView::Reprovision(_)
                )
            })
            .collect();
        assert_eq!(adoption_records.len(), 1);
        assert_eq!(
            adoption_records[0].envelope.operation_id,
            Some(*operation_id.as_bytes())
        );
        assert_eq!(
            adoption_records[0].envelope.idempotency_key,
            Some(*operation_id.as_bytes())
        );
        assert!(FileAuditLog::replay(&handle.paths.audit_dir)
            .unwrap()
            .is_empty());

        handle
            .write_handle()
            .begin_typed_media_readiness_operation(crate::index::MediaReadinessOperationInput {
                operation_id: Uuid::new_v4(),
                run_id: None,
                endpoint: crate::DriveKey::new("LIB001".to_string(), 0x0101),
                changer_sg: None,
                drive_sg: None,
                drive_serial: Some("DRV001".to_string()),
                barcode: Some("ACM901L9".to_string()),
                source_slot: Some(0x0401),
                media_generation: Some(9),
                media_family: Some(crate::LtoGen::Lto9),
                phase: "planned".to_string(),
                state: "planned".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                deadline_at_utc: Some(
                    (time::OffsetDateTime::now_utc() + Duration::minutes(1))
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                ),
                evidence_path: None,
            })
            .expect("record active physical operation guard");
        let guarded = handle
            .adopt_bootstrap_identity(
                adoption_input(Uuid::new_v4()),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("active physical operation evidence must forbid adoption no-op");
        assert!(guarded
            .to_string()
            .contains("active fence, session, readiness"));
    }

    #[test]
    fn bootstrap_adoption_rebuilds_empty_sql_from_typed_source() {
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
        fs::remove_file(temp.path().join("index/rem-state.sqlite")).unwrap();

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        handle.startup_replay().expect("actual startup replay");
        handle
            .rebuild_projection_from_tape_logs()
            .expect("actual live rebuild");
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
                .catalog()
                .get_tape(&adoption_input(operation_id).tape_uuid)
                .expect("lookup")
                .expect("reprojected row")
                .state,
            "recovery_required"
        );
        let paths = handle.paths.clone();
        let authority_before = authority_reset::test_authority_bytes(&paths);
        drop(handle);
        StateHandle::rebuild_projection_from_config_file(&config_path).unwrap();
        assert_eq!(
            authority_reset::test_authority_bytes(&paths),
            authority_before
        );
        let reopened = StateHandle::open_from_config_file(&config_path).unwrap();
        assert_eq!(
            reopened
                .catalog()
                .get_tape(&adoption_input(operation_id).tape_uuid)
                .unwrap()
                .unwrap()
                .state,
            "recovery_required"
        );
    }

    #[test]
    fn bootstrap_adoption_replay_preserves_newer_conflicting_barcode_authority() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        let original = adoption_input(Uuid::new_v4());
        let conflicting_uuid = *Uuid::new_v4().as_bytes();
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
            handle
                .adopt_bootstrap_identity(
                    original.clone(),
                    adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
                )
                .unwrap();
            let mut other = original.clone();
            other.operation_id = Uuid::new_v4();
            other.tape_uuid = conflicting_uuid;
            other.voltag = Some("ACM902L9".into());
            handle
                .adopt_bootstrap_identity(
                    other,
                    adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
                )
                .unwrap();
            handle
                .rebind_tape_label(
                    conflicting_uuid,
                    original.voltag.as_deref().unwrap(),
                    original.tape_uuid,
                )
                .unwrap();
        }
        let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
        handle.startup_replay().unwrap();
        assert!(handle
            .adopt_bootstrap_identity(
                adoption_input(Uuid::new_v4()),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod)
            )
            .is_err());
        assert_eq!(
            handle
                .catalog()
                .get_tape(&conflicting_uuid)
                .unwrap()
                .unwrap()
                .voltag,
            original.voltag
        );
        assert_eq!(
            handle
                .catalog()
                .get_tape(&original.tape_uuid)
                .unwrap()
                .unwrap()
                .voltag,
            None
        );
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
        changed_drive.endpoint = crate::DriveKey::new("LIB001", 0x0102);
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
            .contains("changed immutable facts"));

        let mut different_tape_same_operation = adoption_input(operation_id);
        different_tape_same_operation.tape_uuid[15] ^= 0x01;
        different_tape_same_operation.voltag = Some("ACM902L9".to_string());
        let different_tape_same_operation = handle
            .adopt_bootstrap_identity(
                different_tape_same_operation,
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("one operation UUID cannot bind a different tape");
        assert!(different_tape_same_operation
            .to_string()
            .contains("already bound to a different recorded action"));

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
        {
            use crate::authority::TapeAuthorityView;
            let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
            let input = adoption_input(operation_id);
            let authority = crate::TapeAuthority::open(
                handle.paths.journal_dir.join("tapes"),
                input.tape_uuid,
                &handle.paths.sqlite_path,
            )
            .unwrap();
            let mut lease = authority.acquire_exclusive().unwrap();
            let scheme = match input.parity {
                ParityConfig::None => None,
                ParityConfig::Scheme(v) => Some(v),
            };
            lease
                .publish_initialized(
                    input.block_size,
                    scheme,
                    crate::tape_log::CommitCursor {
                        partition: 0,
                        lba: 2,
                    },
                )
                .unwrap();
            let snapshot = lease.freeze().unwrap();
            handle.index.project_tape_log(&snapshot).unwrap();
            // This ordinary native producer refresh must survive live rebuild.
            handle.rebuild_projection_from_tape_logs().unwrap();
            assert_eq!(
                handle
                    .catalog()
                    .get_tape(&input.tape_uuid)
                    .unwrap()
                    .unwrap()
                    .written_extent_lba,
                Some(2)
            );
        }

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        let error = handle
            .adopt_bootstrap_identity(
                adoption_input(Uuid::new_v4()),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("evolved prefix authority must forbid re-adoption");
        assert!(error
            .to_string()
            .contains("evolved after adoption in native authority"));
        assert_eq!(
            handle
                .catalog()
                .get_tape(&adoption_input(operation_id).tape_uuid)
                .expect("lookup")
                .expect("evolved row")
                .written_extent_lba,
            Some(2)
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
        {
            let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
            handle
                .assign_tape_pool(adoption_input(operation_id).tape_uuid, 1, None)
                .unwrap();
        }

        let mut handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        let error = handle
            .adopt_bootstrap_identity(
                adoption_input(Uuid::new_v4()),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .expect_err("evolved assignment generation must forbid re-adoption");
        assert!(error
            .to_string()
            .contains("assignment or media generation evolved"));
    }

    #[test]
    fn bootstrap_adoption_checks_uuid_and_barcode_conflicts_across_tape_kinds() {
        for same_uuid in [true, false] {
            use crate::library_log as lib;
            let temp = tempfile::tempdir().unwrap();
            let config_path = temp.path().join("config.toml");
            fs::write(&config_path, config_text(temp.path())).unwrap();
            let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
            let mut input = adoption_input(Uuid::new_v4());
            let cartridge = lib::cleaning_cartridge_ref(&input.voltag.as_deref().unwrap().into());
            if same_uuid {
                input.tape_uuid = cartridge.uuid;
            }
            handle
                .library
                .append(|a, v| {
                    a.cleaning_recorded(
                        lib::CleaningRecorded {
                            run_id: "register-cleaning".into(),
                            previous: None,
                            drive_uuid: None,
                            cartridge: Some(cartridge),
                            action: lib::CleaningAction::Register(lib::Register {
                                barcode: input.voltag.as_deref().unwrap().into(),
                                home_slot: 4,
                            }),
                        },
                        AuditActor::System,
                        SourceLayer::Layer4,
                        v,
                    )
                })
                .unwrap();
            assert!(handle
                .adopt_bootstrap_identity(
                    input,
                    adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod)
                )
                .is_err());
        }
    }

    /// Adoption refusals expose an optional barcode as ordinary operator text.
    #[test]
    fn bootstrap_adoption_fresh_pool_intent_preserves_asymmetric_rule() {
        for original_explicit in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let config_path = temp.path().join("config.toml");
            fs::write(&config_path, config_text(temp.path())).unwrap();
            let mut state = StateHandle::open_from_config_file(&config_path).unwrap();
            let mut original = adoption_input(Uuid::new_v4());
            original.pool_id = original_explicit.then(|| "camera.copy-a".into());
            state
                .adopt_bootstrap_identity(
                    original,
                    adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
                )
                .unwrap();
            let mut retry = adoption_input(Uuid::new_v4());
            retry.pool_id = (!original_explicit).then(|| "camera.copy-a".into());
            let mut changed_endpoint =
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod);
            changed_endpoint.endpoint = crate::DriveKey::new("LIB002", 0x0102);
            changed_endpoint.drive_serial = "DRV002".into();
            let result = state.adopt_bootstrap_identity(retry, changed_endpoint);
            assert_eq!(result.is_ok(), original_explicit);
            let records = state.library.test_source_records();
            let source = records
                .iter()
                .find_map(|r| match r.body_view() {
                    crate::library_log::BodyView::Reprovision(v) => Some(v),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                adoption_assignment_source(source),
                Some(if original_explicit {
                    "operator"
                } else {
                    "rule"
                })
            );
            assert_eq!(
                state
                    .catalog()
                    .get_tape(&adoption_input(Uuid::new_v4()).tape_uuid)
                    .unwrap()
                    .unwrap()
                    .assignment_generation,
                1
            );
        }
    }

    #[test]
    fn bootstrap_adoption_empty_header_and_concurrent_native_rebuild() {
        use crate::authority::TapeAuthorityView;
        use std::sync::{mpsc, Arc, Barrier};
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        let mut state = StateHandle::open_from_config_file(&config_path).unwrap();
        let input = adoption_input(Uuid::new_v4());
        let authority = crate::TapeAuthority::open(
            state.paths.journal_dir.join("tapes"),
            input.tape_uuid,
            &state.paths.sqlite_path,
        )
        .unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        let scheme = match &input.parity {
            ParityConfig::None => None,
            ParityConfig::Scheme(v) => Some(v.clone()),
        };
        lease
            .publish_empty_prefix(input.block_size, scheme.clone())
            .unwrap();
        state
            .library
            .refresh_native(&lease.freeze().unwrap())
            .unwrap();
        state
            .adopt_bootstrap_identity(
                input.clone(),
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .unwrap();
        lease
            .publish_initialized(
                input.block_size,
                scheme,
                crate::tape_log::CommitCursor {
                    partition: 0,
                    lba: 2,
                },
            )
            .unwrap();
        let snapshot = lease.freeze().unwrap();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let runtime = state.library.clone();
        let mut writer = state.write_handle();
        let native_thread = {
            let entered = entered.clone();
            let release = release.clone();
            std::thread::spawn(move || {
                runtime
                    .project_native_before_apply(
                        &mut writer,
                        &snapshot,
                        crate::projector::apply::ApplyPath::Live,
                        || {
                            entered.wait();
                            release.wait();
                        },
                    )
                    .unwrap()
            })
        };
        entered.wait();
        let (started, begun) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let runtime = state.library.clone();
        let mut writer = state.write_handle();
        let rebuild = std::thread::spawn(move || {
            started.send(()).unwrap();
            runtime.rebuild_projection(&mut writer).unwrap();
            done.send(()).unwrap();
        });
        begun.recv().unwrap();
        assert!(matches!(
            finished.recv_timeout(std::time::Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        release.wait();
        native_thread.join().unwrap();
        rebuild.join().unwrap();
        finished.recv().unwrap();
        assert_eq!(
            state
                .catalog()
                .get_tape(&input.tape_uuid)
                .unwrap()
                .unwrap()
                .written_extent_lba,
            Some(2)
        );
        assert!(state
            .adopt_bootstrap_identity(
                input,
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod)
            )
            .is_err());
    }

    #[test]
    fn bootstrap_adoption_rule_refusal_prints_plain_barcode() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
        let mut input = adoption_input(Uuid::new_v4());
        input.voltag = Some("BARCODE".into());
        let error = handle
            .adopt_bootstrap_identity(
                input,
                adoption_evidence(BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod),
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("bootstrap barcode BARCODE does not match any locked tape_pool_rule"),
            "{error}"
        );
        assert!(!error.contains("Some("), "{error}");
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
            let handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .write_handle()
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
        let binding: serde_json::Value = serde_json::from_slice(
            &fs::read(temp.path().join("authority-reset-receipt.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(binding["phase"], "Complete");
        assert_eq!(
            fs::read(archive_dir.join("authority-format")).unwrap(),
            fs::read(temp.path().join("authority-format")).unwrap()
        );
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

        let handle = StateHandle::open_from_config_file(&config_path).expect("reopen state");
        assert!(handle
            .catalog()
            .list_tapes(None, TapeKindFilter::Data)
            .expect("list tapes")
            .is_empty());
        assert_eq!(
            handle
                .catalog()
                .get_tape_pool("camera.copy-a")
                .expect("pool lookup")
                .expect("pool projected")
                .display_name
                .as_deref(),
            Some("Camera copy A")
        );
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
            let handle = StateHandle::open_from_config_file(&config_path).expect("open state");
            handle
                .write_handle()
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
            let index = crate::CatalogReader::open(temp.path().join("index/rem-state.sqlite"))
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
    fn exact_full_reset_archives_schema16_while_scoped_legacy_refuses_without_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        seed_clean_break_reset_catalog(
            temp.path(),
            16,
            &[([0xC0; 16], "ACM020L9"), ([0xC1; 16], "ACM021L9")],
        );
        fs::write(temp.path().join("audit/sentinel"), b"unchanged").unwrap();
        let sql = temp.path().join("index/rem-state.sqlite");
        let before = fs::read(&sql).unwrap();
        assert!(StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &[],
            &["ACM020L9".into(), "ACM021L9".into()]
        )
        .is_err());
        assert!(StateHandle::preflight_catalog_reset_from_config_file(
            &config_path,
            &["ACM020L9".into()],
            &["ACM021L9".into()]
        )
        .is_err());
        assert_eq!(fs::read(&sql).unwrap(), before);
        assert_eq!(
            fs::read(temp.path().join("audit/sentinel")).unwrap(),
            b"unchanged"
        );
        assert!(!temp.path().join("reset-archives").exists());
        StateHandle::reset_catalog_from_config_file(&config_path).unwrap();
        let archive = temp.path().join("reset-archives/reset-000001");
        assert_eq!(fs::read(archive.join("catalog.sqlite")).unwrap(), before);
        assert_eq!(
            fs::read(archive.join("audit/sentinel")).unwrap(),
            b"unchanged"
        );
        let handle = StateHandle::open_from_config_file(&config_path).unwrap();
        assert!(handle
            .catalog()
            .list_tapes(None, TapeKindFilter::All)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn exact_full_erase_replaces_sql_only_schema17_without_inventory_read() {
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
        assert_eq!(
            admitted.source_schema_version, 0,
            "SQL schema is never read"
        );
        assert!(admitted.tapes.is_empty());
        StateHandle::reset_catalog_preserving_with_preflight_token_from_config_file(
            &config_path,
            &[],
            &erase,
            &admitted.preflight_token,
        )
        .expect("reset exact schema-17 source");
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
            let handle =
                StateHandle::open_with_config(paths.clone(), config).expect("open with pool");
            handle
                .write_handle()
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
            let handle =
                StateHandle::open_with_config(paths.clone(), config).expect("reopen with pool");
            assert_eq!(
                handle
                    .catalog()
                    .list_tapes(Some("camera.copy-a"), TapeKindFilter::Data)
                    .expect("pool tapes")
                    .len(),
                1
            );
        }

        let config = parse_config_toml(&config_without_pools(temp.path())).expect("config");
        let handle = StateHandle::open_with_config(paths, config).expect("reopen without pool");

        assert!(handle
            .catalog()
            .get_tape_pool("camera.copy-a")
            .expect("pool lookup")
            .is_none());
        let tapes = handle
            .catalog()
            .list_tapes(None, TapeKindFilter::Data)
            .expect("list tapes");
        assert_eq!(tapes.len(), 1);
        assert_eq!(tapes[0].pool_id, None);
        assert!(handle
            .catalog()
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
            let handle =
                StateHandle::open_with_config(paths.clone(), config).expect("open with pool");
            handle
                .write_handle()
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
        let handle = StateHandle::open_with_config(paths, config).expect("reopen with pool b");
        let tapes = handle
            .catalog()
            .list_tapes(None, TapeKindFilter::Data)
            .expect("list tapes");
        assert_eq!(tapes.len(), 1);
        assert_eq!(tapes[0].pool_id.as_deref(), Some("camera.copy-b"));
        assert!(handle
            .catalog()
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
        assert_eq!(report.object_instances_rebuilt, 0);
        assert_eq!(report.audit_records_replayed, 0);
        assert_eq!(report.journal_records_replayed, 0);
    }

    /// Long finalization is refused before native flock acquisition, with the
    /// operation identity retained in the typed error and no catalog mutation.
    #[test]
    fn b7_fix2_retirement_is_busy_with_running_finalization_id() {
        let temp = tempfile::tempdir().unwrap();
        let config_path = temp.path().join("config.toml");
        fs::write(&config_path, config_text(temp.path())).unwrap();
        let mut handle = StateHandle::open_from_config_file(&config_path).unwrap();
        let tape_uuid = [0xc3; 16];
        handle
            .provision_tape(crate::index::ProvisionTapeInput {
                tape_uuid,
                voltag: "BUSY01L9".into(),
                block_size: 4096,
                parity: ParityConfig::None,
                force: false,
            })
            .unwrap();
        let authority = crate::TapeAuthority::open(
            handle.paths.journal_dir.join("tapes"),
            tape_uuid,
            &handle.paths.sqlite_path,
        )
        .unwrap();
        let operation_id = uuid::Uuid::new_v4();
        let operation = authority.finalization_operation(operation_id).unwrap();
        let lease = authority.acquire_exclusive().unwrap();
        let before = std::time::Instant::now();
        let error = handle
            .retire_tape(crate::index::RetireTapeInput {
                tape_uuid,
                reason: "must refuse".into(),
            })
            .unwrap_err();
        assert!(matches!(error, StateError::TapeBusy { operation_id: id } if id == operation_id));
        assert!(before.elapsed() < std::time::Duration::from_millis(250));
        assert_ne!(
            handle
                .catalog()
                .get_tape(&tape_uuid)
                .unwrap()
                .unwrap()
                .state,
            "retired"
        );
        drop(lease);
        drop(operation);
        authority.check_finalizing().unwrap();
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
            .write_handle()
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
        assert_eq!(outcome.instances_marked_missing, 0);
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
            record.detail.get("instances_marked_missing"),
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
            let handle =
                StateHandle::open_with_config(paths.clone(), config.clone()).expect("open first");
            crate::FileAuditLog::open(&handle.paths().audit_dir, true)
                .unwrap()
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
            crate::FileAuditLog::open(&handle.paths().audit_dir, true)
                .unwrap()
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
            crate::FileAuditLog::open(&handle.paths().audit_dir, true)
                .unwrap()
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
            crate::FileAuditLog::open(&handle.paths().audit_dir, true)
                .unwrap()
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
                .catalog()
                .get_tape(&[0x21; 16])
                .expect("tape lookup")
                .expect("audit-projected tape")
                .state,
            "retired"
        );
        assert_eq!(
            restarted
                .catalog()
                .session_state(session_id)
                .expect("session state")
                .as_deref(),
            Some("closed")
        );
    }

    /// Build ordinary bay-session audit evidence for startup projection gaps.
    fn library_restart_event(
        session_id: Uuid,
        tape_uuid: [u8; 16],
        event: AuditEvent,
    ) -> AuditEventRecord {
        AuditEventRecord {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: None,
            session_id: Some(session_id),
            idempotency_key: None,
            event,
            subject: AuditSubject {
                kind: "write".into(),
                id: Some(session_id.to_string()),
            },
            detail: BTreeMap::from([
                ("session_kind".into(), CborValue::Text("write".into())),
                ("tape_uuid".into(), CborValue::Bytes(tape_uuid.to_vec())),
                ("library_serial".into(), CborValue::Text("LIB001".into())),
                ("drive_bay".into(), CborValue::Integer(256.into())),
            ]),
        }
    }

    /// A durable close wins over stale open projection without invalidating its tape.
    #[test]
    fn library_startup_replays_unprojected_session_close() {
        let temp = tempfile::tempdir().unwrap();
        let config = parse_config_toml(&config_text(temp.path())).unwrap();
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let session_id = Uuid::new_v4();
        let tape_uuid = [0x71; 16];
        {
            let mut state = StateHandle::open_with_config(paths.clone(), config.clone()).unwrap();
            let (_, opened) = crate::FileAuditLog::open(&state.paths().audit_dir, true)
                .unwrap()
                .append_and_return_record(library_restart_event(
                    session_id,
                    tape_uuid,
                    AuditEvent::SessionOpened,
                ))
                .unwrap();
            state.index.project_audit_record(&opened).unwrap();
            state
                .calibration_control()
                .record_harvest_success(tape_uuid, 0)
                .unwrap();
            crate::FileAuditLog::open(&state.paths().audit_dir, true)
                .unwrap()
                .append(library_restart_event(
                    session_id,
                    tape_uuid,
                    AuditEvent::SessionClosed,
                ))
                .unwrap();
        }
        let _lock = StateLockGuard::acquire(&paths.state_dir).unwrap();
        let mut index = crate::projector::open(&paths.sqlite_path)
            .map(|catalog| catalog.write_handle())
            .unwrap();
        assert_eq!(
            index.session_state(session_id).unwrap().as_deref(),
            Some("open")
        );
        let calibration_path = paths
            .calibration_dir
            .join(crate::CALIBRATION_CONTROL_FILENAME);
        let calibration_before = fs::read(&calibration_path).unwrap();

        let lost = recover_library_sessions_after_restart(&mut index, &config).unwrap();

        assert_eq!(
            index.session_state(session_id).unwrap().as_deref(),
            Some("closed")
        );
        assert_eq!(lost, 0);
        assert_eq!(fs::read(calibration_path).unwrap(), calibration_before);
        assert!(!FileAuditLog::replay(&paths.audit_dir)
            .unwrap()
            .iter()
            .any(|record| record.event == AuditEvent::SessionLostByRestart));
    }

    /// A durable open missing from SQLite still belongs to startup recovery.
    #[test]
    fn library_startup_replays_unprojected_session_open() {
        let temp = tempfile::tempdir().unwrap();
        let config = parse_config_toml(&config_text(temp.path())).unwrap();
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let session_id = Uuid::new_v4();
        {
            let state = StateHandle::open_with_config(paths.clone(), config.clone()).unwrap();
            crate::FileAuditLog::open(&state.paths().audit_dir, true)
                .unwrap()
                .append(library_restart_event(
                    session_id,
                    [0x72; 16],
                    AuditEvent::SessionOpened,
                ))
                .unwrap();
        }
        let _lock = StateLockGuard::acquire(&paths.state_dir).unwrap();
        let mut index = crate::projector::open(&paths.sqlite_path)
            .map(|catalog| catalog.write_handle())
            .unwrap();
        assert_eq!(index.session_state(session_id).unwrap(), None);

        let lost = recover_library_sessions_after_restart(&mut index, &config).unwrap();

        assert_eq!(
            index.session_state(session_id).unwrap().as_deref(),
            Some("lost_by_restart")
        );
        assert_eq!(lost, 1);
    }

    /// Daemon recovery scopes loss and existing volume invalidation to its library bays.
    #[test]
    fn library_startup_recovery_is_scoped_and_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = parse_config_toml(&config_text(temp.path())).unwrap();
        let mut second_library = config.libraries[0].clone();
        second_library.serial = "LIB002".into();
        config.libraries.push(second_library);
        let paths = StatePaths::from_config(temp.path().join("config.toml"), &config);
        let sessions = [
            (
                Some(crate::DriveKey::new("LIB001", 256)),
                "write",
                "lost_by_restart",
            ),
            (
                Some(crate::DriveKey::new("LIB001", 0)),
                "read",
                "lost_by_restart",
            ),
            (Some(crate::DriveKey::new("OTHER", 256)), "write", "open"),
            (
                Some(crate::DriveKey::Standalone {
                    drive_serial: "DIRECT".into(),
                }),
                "write",
                "open",
            ),
            (Some(crate::DriveKey::new("LIB001", 257)), "write", "closed"),
            (None, "write", "open"),
            (
                Some(crate::DriveKey::new("LIB002", 256)),
                "write",
                "lost_by_restart",
            ),
        ];
        {
            let mut state = StateHandle::open_with_config(paths.clone(), config.clone()).unwrap();
            for (n, (endpoint, kind, expected)) in sessions.iter().enumerate() {
                let id = Uuid::from_u128(n as u128 + 1);
                let tape = [n as u8 + 1; 16];
                let mut detail = BTreeMap::from([
                    ("session_kind".into(), CborValue::Text((*kind).into())),
                    ("tape_uuid".into(), CborValue::Bytes(tape.to_vec())),
                ]);
                if let Some(endpoint) = endpoint {
                    // The read fixture exercises legacy bay-coordinate evidence.
                    if *kind != "read" {
                        detail.insert("endpoint_key".into(), CborValue::Text(endpoint.encode()));
                    }
                    // Explicit standalone identity wins over stale bay coordinates.
                    detail.insert(
                        "library_serial".into(),
                        CborValue::Text(endpoint.library_serial().unwrap_or("LIB001").into()),
                    );
                    detail.insert(
                        "drive_bay".into(),
                        CborValue::Integer(endpoint.bay().unwrap_or(256).into()),
                    );
                }
                let mut event = AuditEventRecord {
                    actor: AuditActor::System,
                    source_layer: SourceLayer::Layer5,
                    operation_id: None,
                    session_id: Some(id),
                    idempotency_key: None,
                    event: AuditEvent::SessionOpened,
                    subject: AuditSubject {
                        kind: (*kind).into(),
                        id: Some(id.to_string()),
                    },
                    detail,
                };
                crate::FileAuditLog::open(&state.paths().audit_dir, true)
                    .unwrap()
                    .append(event.clone())
                    .unwrap();
                if *expected == "closed" {
                    event.event = AuditEvent::SessionClosed;
                    crate::FileAuditLog::open(&state.paths().audit_dir, true)
                        .unwrap()
                        .append(event)
                        .unwrap();
                }
                state
                    .calibration_control()
                    .record_harvest_success(tape, 0)
                    .unwrap();
            }
            state.replay_audit_projection().unwrap();
        }
        let _lock = StateLockGuard::acquire(&paths.state_dir).unwrap();
        let mut index = crate::projector::open(&paths.sqlite_path)
            .map(|catalog| catalog.write_handle())
            .unwrap();
        assert_eq!(
            recover_library_sessions_after_restart(&mut index, &config).unwrap(),
            3
        );
        let calibration = CalibrationControlStore::open(&paths.calibration_dir).unwrap();
        for (n, (_, kind, expected)) in sessions.iter().enumerate() {
            assert_eq!(
                index
                    .session_state(Uuid::from_u128(n as u128 + 1))
                    .unwrap()
                    .as_deref(),
                Some(*expected)
            );
            let row = calibration.row([n as u8 + 1; 16]);
            let invalidated = *kind == "write" && *expected == "lost_by_restart";
            assert_eq!(row.write_epoch, u64::from(invalidated));
            assert_eq!(
                row.state == crate::VolumeCalibrationState::Uncalibrated,
                invalidated
            );
            assert!(!index
                .tape_io_is_fenced(Some(&[n as u8 + 1; 16]), None, None)
                .unwrap());
        }
        let before = FileAuditLog::replay(&paths.audit_dir).unwrap().len();
        let calibration_bytes = fs::read(
            paths
                .calibration_dir
                .join(crate::CALIBRATION_CONTROL_FILENAME),
        )
        .unwrap();
        assert_eq!(
            recover_library_sessions_after_restart(&mut index, &config).unwrap(),
            0
        );
        assert_eq!(
            FileAuditLog::replay(&paths.audit_dir).unwrap().len(),
            before
        );
        assert_eq!(
            fs::read(
                paths
                    .calibration_dir
                    .join(crate::CALIBRATION_CONTROL_FILENAME)
            )
            .unwrap(),
            calibration_bytes
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
            let handle =
                StateHandle::open_with_config(paths.clone(), config.clone()).expect("open first");
            crate::FileAuditLog::open(&handle.paths().audit_dir, true)
                .unwrap()
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
            crate::FileAuditLog::open(&handle.paths().audit_dir, true)
                .unwrap()
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
            crate::FileAuditLog::open(&handle.paths().audit_dir, true)
                .unwrap()
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
                .catalog()
                .operation_state(operation_id)
                .expect("operation state")
                .as_deref(),
            Some("failed")
        );
        assert_eq!(
            restarted
                .catalog()
                .session_state(session_id)
                .expect("session state")
                .as_deref(),
            Some("lost_by_restart")
        );
        assert_eq!(
            restarted
                .catalog()
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
        let handle = StateHandle::open_from_config_file(&config_path).expect("open state");
        let tape_uuid = [0x42u8; 16];

        let record = wrap_map_record(tape_uuid, 3, 17);
        handle
            .write_handle()
            .upsert_wrap_map(&record)
            .expect("upsert wrap map");
        let fetched = handle
            .catalog()
            .get_wrap_map(&tape_uuid)
            .expect("get wrap map")
            .expect("row present");
        assert_eq!(fetched, record, "raw descriptors round-trip unchanged");

        // Replacement at the next load harvest overwrites in place.
        let extended = wrap_map_record(tape_uuid, 4, 21);
        handle
            .write_handle()
            .upsert_wrap_map(&extended)
            .expect("replace wrap map");
        let fetched = handle
            .catalog()
            .get_wrap_map(&tape_uuid)
            .expect("get wrap map")
            .expect("row present");
        assert_eq!(fetched.write_epoch, 4);

        assert!(handle
            .write_handle()
            .delete_wrap_map(&tape_uuid)
            .expect("delete"));
        assert!(handle
            .catalog()
            .get_wrap_map(&tape_uuid)
            .expect("get after delete")
            .is_none());
        assert!(
            !handle
                .write_handle()
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
            .write_handle()
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
            .expect_err("packet tag4 forbids geometry replacement under the same UUID");
        let after_geometry_change = handle.calibration_control().row(old_uuid);
        assert_eq!(
            after_geometry_change.state,
            crate::calibration::VolumeCalibrationState::Calibrated
        );
        assert_eq!(
            after_geometry_change.calibration_generation,
            calibration_generation
        );
        assert!(handle
            .catalog()
            .get_wrap_map(&old_uuid)
            .expect("map lookup after refused geometry change")
            .is_some());

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
            .write_handle()
            .upsert_wrap_map(&wrap_map_record(
                old_uuid,
                write_epoch,
                recalibrated_generation,
            ))
            .expect("store replacement wrap map");

        handle
            .retire_tape(RetireTapeInput {
                tape_uuid: old_uuid,
                reason: "confirmed replacement".into(),
            })
            .unwrap();
        handle
            .provision_replacing_media(
                ProvisionTapeInput {
                    tape_uuid: new_uuid,
                    voltag: "ACM045L9".to_string(),
                    block_size: 512 * 1024,
                    parity: ParityConfig::None,
                    force: true,
                },
                crate::LabelProvenance::Changer,
                crate::MediaObservation::label_hint("ACM045L9"),
                Some(old_uuid),
            )
            .expect("reprovision physical identity");

        assert!(handle
            .catalog()
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
            .write_handle()
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
                .catalog()
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
            let handle = StateHandle::open_from_config_file(&config_path).expect("open state");
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
                .write_handle()
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

        let handle = StateHandle::open_from_config_file(&config_path).expect("reopen");
        assert!(
            handle
                .catalog()
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
            let handle =
                StateHandle::open_with_config(paths.clone(), config.clone()).expect("open");
            for (session_id, kind, tape) in [
                (write_session, "write", write_tape),
                (read_session, "read", read_tape),
            ] {
                crate::FileAuditLog::open(&handle.paths().audit_dir, true)
                    .unwrap()
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

#[cfg(test)]
mod cutover_tests {
    use super::*;
    use crate::authority::{fixtures::AuthorityBuilder, TapeAuthorityWriter};

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        fs::write(&config, super::tests::config_text(root.path())).unwrap();
        (root, config)
    }
    fn hash_files(path: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        if !path.exists() {
            return;
        }
        if path.is_file() {
            out.insert(
                path.to_path_buf(),
                Sha256::digest(fs::read(path).unwrap()).to_vec(),
            );
            return;
        }
        for entry in fs::read_dir(path).unwrap() {
            hash_files(&entry.unwrap().path(), out);
        }
    }

    /// Every file and directory below `path`, sorted.
    fn tree(path: &Path) -> Vec<PathBuf> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap().path();
            if entry.is_dir() {
                entries.extend(tree(&entry));
            }
            entries.push(entry);
        }
        entries.sort();
        entries
    }

    /// Read-only validation succeeds beside the daemon's lifetime state lock and
    /// creates nothing: no lock file, marker or journal namespace.
    #[test]
    fn layout_validation_is_read_only_beside_the_owner_state_lock() {
        let (root, config_path) = setup();
        let config = crate::load_config(&config_path).unwrap();
        let before = tree(root.path());
        crate::authority_layout::validate_config_layout(&config).unwrap();
        crate::authority_layout::load_runtime_config(&config_path).unwrap();
        assert_eq!(tree(root.path()), before, "an empty state stays empty");
        assert!(!root.path().join("state.lock").exists());
        assert!(!root.path().join("authority-format").exists());

        let state = StateHandle::open_from_config_file(&config_path).unwrap();
        crate::authority_layout::validate_owned_config_layout(state.config()).unwrap();
        let before = tree(root.path());
        crate::authority_layout::validate_config_layout(state.config()).unwrap();
        crate::authority_layout::load_runtime_config(&config_path).unwrap();
        assert_eq!(tree(root.path()), before);
        assert!(StateLockGuard::acquire(&state.paths.state_dir)
            .unwrap_err()
            .is_state_lock_held());

        // A marker-less, non-empty state is still refused without installing one.
        let paths = state.paths.clone();
        drop(state);
        fs::remove_file(paths.state_dir.join("authority-format")).unwrap();
        assert!(crate::authority_layout::validate_config_layout(&config)
            .unwrap_err()
            .to_string()
            .contains(crate::authority_layout::RESET_REQUIRED));
        assert!(!paths.state_dir.join("authority-format").exists());
    }

    #[test]
    fn owned_runtime_installs_marker_before_catalog_and_writes_no_tape_log() {
        let (_root, config_path) = setup();
        let config = crate::load_config(&config_path).unwrap();
        let paths = StatePaths::from_config(&config_path, &config);
        let lock = StateLockGuard::acquire(&paths.state_dir).unwrap();
        crate::authority_layout::admit_owned_config(&config).unwrap();
        assert!(paths.state_dir.join("authority-format").is_file());
        assert!(!paths.sqlite_path.exists());
        assert_eq!(
            fs::read_dir(paths.journal_dir.join("tapes"))
                .unwrap()
                .count(),
            0
        );
        drop(
            crate::projector::open(&paths.sqlite_path)
                .map(|catalog| catalog.write_handle())
                .unwrap(),
        );
        crate::authority_layout::admit_owned_config(&config).unwrap();
        drop(lock);
        StateHandle::open_from_config_file(&config_path).unwrap();
    }

    #[test]
    fn authority_layout_refuses_unmarked_schema22_and_marked_mixed_authority() {
        let (_root, config) = setup();
        let state = StateHandle::open_from_config_file(&config).unwrap();
        let paths = state.paths.clone();
        assert!(paths.state_dir.join("authority-format").exists());
        drop(state);
        fs::remove_file(paths.state_dir.join("authority-format")).unwrap();
        let conn = rusqlite::Connection::open(&paths.sqlite_path).unwrap();
        conn.pragma_update(None, "user_version", 22).unwrap();
        drop(conn);
        assert!(StateHandle::open_from_config_file(&config)
            .unwrap_err()
            .to_string()
            .contains(crate::authority_layout::RESET_REQUIRED));
        assert!(crate::projector::open(&paths.sqlite_path)
            .map(|catalog| catalog.write_handle())
            .unwrap_err()
            .to_string()
            .contains(crate::authority_layout::RESET_REQUIRED));
        crate::authority_layout::install(&paths, true).unwrap();
        fs::write(paths.journal_dir.join("old.remjournal"), b"old authority").unwrap();
        assert!(StateHandle::open_from_config_file(&config)
            .unwrap_err()
            .to_string()
            .contains(crate::authority_layout::RESET_REQUIRED));
    }

    #[test]
    /// Restart after SQLite file creation is safe; unversioned user data is refused.
    fn authority_layout_restarts_an_empty_unversioned_catalog_only() {
        for marked in [false, true] {
            for populated in [false, true] {
                let (_root, config_path) = setup();
                let config = crate::load_config(&config_path).unwrap();
                let paths = StatePaths::from_config(&config_path, &config);
                let lock = StateLockGuard::acquire(&paths.state_dir).unwrap();
                crate::authority_layout::admit_owned_config(&config).unwrap();
                ensure_state_directories(&paths).unwrap();
                let marker = paths.state_dir.join("authority-format");
                if !marked {
                    fs::remove_file(&marker).unwrap();
                }
                let conn = rusqlite::Connection::open(&paths.sqlite_path).unwrap();
                conn.execute_batch("pragma user_version = 0;").unwrap();
                if populated {
                    conn.execute_batch("create table existing_data(value text);")
                        .unwrap();
                }
                drop(conn);
                drop(lock);
                let before = fs::read(&paths.sqlite_path).unwrap();
                let validated = crate::authority_layout::validate_config_layout(&config);
                assert_eq!(fs::read(&paths.sqlite_path).unwrap(), before);
                if populated {
                    assert!(validated
                        .unwrap_err()
                        .to_string()
                        .contains(crate::authority_layout::RESET_REQUIRED));
                } else {
                    validated.expect("runtime validation admits an empty catalog read-only");
                }
                let restarted = StateHandle::open_from_config_file(&config_path);
                if populated {
                    assert!(restarted
                        .unwrap_err()
                        .to_string()
                        .contains(crate::authority_layout::RESET_REQUIRED));
                    assert_eq!(fs::read(&paths.sqlite_path).unwrap(), before);
                    assert_eq!(marker.exists(), marked);
                } else {
                    let state = restarted.expect("restart initializes the empty catalog");
                    assert_eq!(
                        state.index.schema_version().unwrap(),
                        crate::index::SCHEMA_VERSION
                    );
                    assert!(fs::read_to_string(&marker)
                        .unwrap()
                        .starts_with("remanence-tape-log-v2\n"));
                }
            }
        }
    }

    #[test]
    /// Reset a historical catalog, preserving archived authority bytes exactly.
    fn v2_reset_genuine_schema23_catalog_preserves_archive_bytes() {
        let (_root, config_path) = setup();
        let config = crate::load_config(&config_path).unwrap();
        let paths = StatePaths::from_config(&config_path, &config);
        let lock = StateLockGuard::acquire(&paths.state_dir).unwrap();
        crate::authority_layout::admit_owned_config(&config).unwrap();
        ensure_state_directories(&paths).unwrap();
        let marker = fs::read(paths.state_dir.join("authority-format")).unwrap();
        let conn = rusqlite::Connection::open(&paths.sqlite_path).unwrap();
        conn.execute_batch(include_str!("../tests/fixtures/schema23.sql"))
            .unwrap();
        let has_column: bool = conn.query_row(
            "select exists(select 1 from pragma_table_info('tapes') where name = 'unprotected_since')",
            [], |row| row.get(0),
        ).unwrap();
        assert!(!has_column, "the genuine predecessor lacks the v24 column");
        conn.execute(
            "insert into tapes(tape_uuid, voltag, block_size, state, updated_at_utc)
             values(?1, 'SCHEMA23L9', 262144, 'ready', '2026-11-02T00:00:00Z')",
            [[0x23u8; 16].as_slice()],
        )
        .unwrap();
        assert_eq!(
            conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
                .unwrap(),
            23
        );
        drop(conn);
        let journal = b"schema-23 predecessor authority\x00\xff";
        let audit = b"schema-23 predecessor audit\x00\xfe";
        fs::write(paths.journal_dir.join("old.remjournal"), journal).unwrap();
        fs::write(paths.audit_dir.join("old.audit"), audit).unwrap();
        drop(lock);
        let before = fs::read(&paths.sqlite_path).unwrap();
        assert!(StateHandle::open_from_config_file(&config_path)
            .unwrap_err()
            .to_string()
            .contains("rem reset-catalog"));
        assert_eq!(fs::read(&paths.sqlite_path).unwrap(), before);
        StateHandle::reset_catalog_from_config_file(&config_path).unwrap();
        let archive = paths.state_dir.join("reset-archives/reset-000001");
        assert_eq!(
            fs::read(archive.join("journals/old.remjournal")).unwrap(),
            journal
        );
        assert_eq!(fs::read(archive.join("audit/old.audit")).unwrap(), audit);
        assert_eq!(fs::read(archive.join("authority-format")).unwrap(), marker);
        let state = StateHandle::open_from_config_file(&config_path).unwrap();
        assert_eq!(
            state.index.schema_version().unwrap(),
            crate::index::SCHEMA_VERSION
        );
        assert!(state
            .index
            .list_tapes(None, crate::TapeKindFilter::All)
            .unwrap()
            .is_empty());
        assert!(!paths.journal_dir.join("old.remjournal").exists());
        assert!(!paths.audit_dir.join("old.audit").exists());
    }

    #[test]
    fn v2_reset_archives_each_predecessor_authority_without_reading_it() {
        for layout in ["two-journal", "v1", "schema23", "v1-header"] {
            let (_root, config) = setup();
            let state = StateHandle::open_from_config_file(&config).unwrap();
            let paths = state.paths.clone();
            drop(state);
            let marker_path = paths.state_dir.join("authority-format");
            let mut marker = fs::read_to_string(&marker_path).unwrap();
            if layout == "v1" {
                marker = marker.replace("remanence-tape-log-v2", "remanence-tape-log-v1");
                fs::write(&marker_path, &marker).unwrap();
            }
            if layout == "schema23" {
                let conn = rusqlite::Connection::open(&paths.sqlite_path).unwrap();
                // A predecessor fixture must use its historical table shape,
                // not just relabel the current schema version.
                conn.execute_batch("alter table object_instances rename to object_copies; alter table object_copies add column pool_id text;").unwrap();
                conn.pragma_update(None, "user_version", 23).unwrap();
            }
            let relative = if layout == "two-journal" {
                "old.remjournal"
            } else {
                "tapes/11111111111111111111111111111111.remtape"
            };
            let bytes: &[u8] = if layout == "v1-header" {
                b"REMTAPE\x01\x01\x00predecessor header under a v2 marker"
            } else {
                b"predecessor authority must be archived without a compatibility reader"
            };
            fs::write(paths.journal_dir.join(relative), bytes).unwrap();
            if layout == "two-journal" {
                fs::remove_file(&marker_path).unwrap();
            }
            assert!(StateHandle::open_from_config_file(&config)
                .unwrap_err()
                .to_string()
                .contains(crate::authority_layout::RESET_REQUIRED));
            StateHandle::reset_catalog_from_config_file(&config).unwrap();
            let archive = paths.state_dir.join("reset-archives/reset-000001");
            assert_eq!(
                fs::read(archive.join("journals").join(relative)).unwrap(),
                bytes
            );
            if layout != "two-journal" {
                assert_eq!(
                    fs::read_to_string(archive.join("authority-format")).unwrap(),
                    marker
                );
            }
            assert!(fs::read_to_string(marker_path)
                .unwrap()
                .starts_with("remanence-tape-log-v2\n"));
            assert!(!paths.journal_dir.join(relative).exists());
            StateHandle::open_from_config_file(&config).unwrap();
        }
    }

    /// Ruling 32-L1: tail repair refuses before it reads, replays, archives or writes anything, so
    /// a torn tail on a projected tape is left exactly as found, with and without a rebuild.
    #[test]
    fn repair_is_refused_before_any_effect_and_leaves_the_projection_ready() {
        use crate::authority::{TapeAuthorityView, TapeAuthorityWriter};
        use std::io::Write;
        let (_root, config) = setup();
        let mut state = StateHandle::open_from_config_file(&config).unwrap();
        let id = [0x76; 16];
        let directory = state.paths.journal_dir.join("tapes");
        let authority =
            crate::TapeAuthority::open(&directory, id, &state.paths.sqlite_path).unwrap();
        let mut writer = authority.acquire_exclusive().unwrap();
        writer
            .publish_report(&(&crate::checkpoint::tests::record(id)).into())
            .unwrap();
        let snapshot = writer.freeze().unwrap();
        state.index.project_tape_log(&snapshot).unwrap();
        let through = state.index.projected_through_sequence(id).unwrap();
        drop(writer);
        let path = directory.join(format!("{}.remtape", uuid::Uuid::from_bytes(id).simple()));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"torn")
            .unwrap();
        drop(state);
        let mut state = StateHandle::open_from_config_file(&config).unwrap();
        let tape_state = state.index.get_tape(&id).unwrap().unwrap().state;
        assert_eq!(tape_state, "ready");
        let mut before = BTreeMap::new();
        for root in [&state.paths.journal_dir, &state.paths.audit_dir] {
            hash_files(root, &mut before);
        }
        for rebuild in [false, true] {
            let error = state
                .repair_tape_log_tail(id, through, rebuild)
                .unwrap_err();
            assert!(error.to_string().contains("TailRepairDeferred"), "{error}");
            let mut after = BTreeMap::new();
            for root in [&state.paths.journal_dir, &state.paths.audit_dir] {
                hash_files(root, &mut after);
            }
            assert_eq!(after, before, "no archive, fence or byte changed");
            assert!(!path.with_extension("repair-required").exists());
            assert_eq!(
                state.index.get_tape(&id).unwrap().unwrap().state,
                tape_state
            );
            assert_eq!(state.index.projected_through_sequence(id).unwrap(), through);
        }
    }

    /// Ruling 32-L1: a log whose seal or decision is corrupt is not repaired either; the refusal
    /// comes before any authority or catalog row is touched.
    #[test]
    fn repair_refuses_corrupt_seal_or_decision_before_changing_authority() {
        use crate::authority::{TapeAuthorityView, TapeAuthorityWriter};
        use crate::tape_log::Record;
        for cut_seal in [false, true] {
            let (_root, config) = setup();
            let mut state = StateHandle::open_from_config_file(&config).unwrap();
            let id = [0x74; 16];
            let directory = state.paths.journal_dir.join("tapes");
            let authority =
                crate::TapeAuthority::open(&directory, id, &state.paths.sqlite_path).unwrap();
            let mut writer = authority.acquire_exclusive().unwrap();
            writer
                .publish_report(&(&crate::checkpoint::tests::record(id)).into())
                .unwrap();
            writer
                .begin_finalization(&(&crate::checkpoint::tests::finalization_intent(id)).into())
                .unwrap();
            let mut progress = crate::TerminalFinalizationProgress::BeforeReplicaA;
            while let Some(next) = progress.successor() {
                writer.advance_finalization(progress, next).unwrap();
                progress = next;
            }
            writer.seal(|| Ok(())).unwrap();
            let snapshot = writer.freeze().unwrap();
            state.index.project_tape_log(&snapshot).unwrap();
            let mut decoder = snapshot.raw.decoder().unwrap();
            let mut cut = None;
            while let Some(frame) = decoder.next_row_frame().unwrap() {
                if (cut_seal && matches!(frame.record, Record::Sealed { .. }))
                    || (!cut_seal && matches!(frame.record, Record::FinalizationDecided(_)))
                {
                    cut = Some(frame.offset);
                    break;
                }
            }
            drop(writer);
            let path = directory.join(format!("{}.remtape", uuid::Uuid::from_bytes(id).simple()));
            let mut bytes = fs::read(&path).unwrap();
            bytes[cut.unwrap() as usize + 28] ^= 0x80;
            fs::write(&path, &bytes).unwrap();
            let store =
                crate::tape_log::store::Store::open(&directory, id, &state.paths.sqlite_path)
                    .unwrap();
            let through = store.inspect_tail().unwrap().through_sequence;
            let mut before = BTreeMap::new();
            hash_files(&state.paths.journal_dir, &mut before);
            let error = state.repair_tape_log_tail(id, through, true).unwrap_err();
            assert!(error.to_string().contains("TailRepairDeferred"), "{error}");
            let mut after = BTreeMap::new();
            hash_files(&state.paths.journal_dir, &mut after);
            assert_eq!(after, before, "no archive, fence or byte changed");
            assert_eq!(fs::read(&path).unwrap(), bytes);
            assert_eq!(state.index.get_tape(&id).unwrap().unwrap().state, "sealed");
            assert!(authority.acquire_for_tail_reconciliation().is_err());
        }
    }
    #[test]
    fn projection_rebuild_hashes_authority_audit_and_never_resets() {
        let (_root, config) = setup();
        let mut state = StateHandle::open_from_config_file(&config).unwrap();
        let id = [0x69; 16];
        let authority = AuthorityBuilder::new(state.paths.journal_dir.join("tapes"), id)
            .build()
            .unwrap();
        let mut lease = authority.acquire_exclusive().unwrap();
        lease
            .publish_report(&(&crate::checkpoint::tests::record(id)).into())
            .unwrap();
        drop(lease);
        state.calibration.record_harvest_success(id, 0).unwrap();
        let mut before = BTreeMap::new();
        for path in [&state.paths.journal_dir, &state.paths.audit_dir] {
            hash_files(path, &mut before);
        }
        REJECT_RESET_HELPER.with(|flag| flag.set(true));
        state.rebuild_projection_from_tape_logs().unwrap();
        state.rebuild_projection_from_tape_logs().unwrap();
        let fault = rusqlite::Connection::open(&state.paths.sqlite_path).unwrap();
        fault.execute_batch("create trigger interrupt_projection before insert on tape_log_projection begin select raise(abort, 'interrupted rebuild'); end").unwrap();
        assert!(state.rebuild_projection_from_tape_logs().is_err());
        assert_eq!(state.index.projected_through_sequence(id).unwrap(), 5);
        fault
            .execute_batch("drop trigger interrupt_projection")
            .unwrap();
        state.rebuild_projection_from_tape_logs().unwrap();
        REJECT_RESET_HELPER.with(|flag| flag.set(false));
        let mut after = BTreeMap::new();
        for path in [&state.paths.journal_dir, &state.paths.audit_dir] {
            hash_files(path, &mut after);
        }
        assert_eq!(before, after);
        assert_eq!(state.index.projected_through_sequence(id).unwrap(), 5);
    }
}

mod initialization;
pub use initialization::InitializationGuard;
