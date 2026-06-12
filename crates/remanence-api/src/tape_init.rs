//! Pure tape-init classification and decision helpers.
//!
//! The hardware orchestration that reads element status, loads drives, reads
//! BOT, and writes the bootstrap stays outside this module. Callers project
//! those facts into these value types so the destructive init decision can be
//! unit-tested without a drive.

use remanence_parity::{
    bootstrap::{parse_bootstrap_block, BootstrapPayload},
    ParityConfig, ParityScheme, SchemeId,
};
use remanence_state::{CatalogIndex, StateError, TapeRecord};

use crate::{build_tape_bootstrap, write_tape_bootstrap, PoolWriteError, TapeUuid};

const BOT_CLASSIFY_READ_BYTES: usize = 1024 * 1024;

/// Physical tape geometry carried by a Remanence bootstrap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeInitGeometry {
    /// Fixed tape block size recorded in the bootstrap.
    pub block_size_bytes: u32,
    /// Parity mode recorded in the bootstrap.
    pub parity: ParityConfig,
}

/// BOT classification projected from the physical read path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BotClassification {
    /// The drive reported BLANK CHECK/EOD immediately at BOT.
    BlankCheckEod,
    /// BOT contains a Remanence bootstrap.
    OursBootstrap {
        /// Tape UUID parsed from the bootstrap.
        uuid: TapeUuid,
        /// Bootstrap geometry parsed from the BOT block.
        geometry: TapeInitGeometry,
    },
    /// BOT contains a known non-Remanence format.
    ForeignFormat {
        /// Human-readable format id.
        name: String,
    },
    /// BOT read failed and media state is unknown.
    ReadError,
    /// BOT returned readable bytes, but no known signature matched.
    UnrecognizedData,
}

/// BOT classification plus the physical-data-after-bootstrap projection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotInitProjection {
    /// BOT classification used by the pure init decision.
    pub classification: BotClassification,
    /// True when readable data, or an ambiguity while checking for it, exists
    /// past a valid Remanence bootstrap.
    pub physical_data_past_bootstrap: bool,
}

/// Barcode lifecycle projected from the operator/catalog state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarcodeLifecycleState {
    /// The barcode is available to assign to a newly initialized tape.
    Available,
    /// The barcode is already assigned to the given tape UUID.
    AssignedTo(TapeUuid),
    /// The barcode has been retired and must not be assigned again.
    Retired,
}

/// Relationship between the physical barcode and the matched catalog row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CatalogBarcodeRelation {
    /// Physical barcode matches the catalog row.
    Matches,
    /// Physical barcode differs, but an explicit relabel record explains it.
    DiffersWithRecordedRelabel,
    /// Physical barcode differs without an explicit relabel record.
    DiffersWithoutRelabel,
}

/// Lifecycle disposition of the catalog row resolved for a BOT uuid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogRowDisposition {
    /// The row is a live identity that the init gauntlet must protect.
    Active,
    /// The row was permanently retired; its medium is free to re-init.
    Retired,
}

/// Catalog row matched for the init decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogTapeInitRow {
    /// Tape UUID from the matched catalog row.
    pub uuid: TapeUuid,
    /// Catalog geometry for this tape.
    pub geometry: TapeInitGeometry,
    /// True when the catalog has no committed tape files for this tape.
    pub catalog_unwritten: bool,
    /// Barcode-to-catalog relationship from the identity layer.
    pub barcode_relation: CatalogBarcodeRelation,
    /// Whether this identity is live or permanently retired.
    pub disposition: CatalogRowDisposition,
}

/// Catalog facts projected for the pure tape-init decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeInitCatalogProjection {
    /// Catalog row for the physical tape UUID, when BOT supplied one.
    pub catalog_row: Option<CatalogTapeInitRow>,
    /// Barcode lifecycle projected from current catalog rows.
    pub barcode_state: BarcodeLifecycleState,
    /// Committed-copy snapshot state for the tape under consideration.
    pub committed_copies: CommittedCopyState,
    /// Non-fatal projection notes for deferred identity features.
    pub notes: Vec<String>,
}

/// Committed-copy snapshot state for the tape under consideration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommittedCopyState {
    /// No committed copies reference the tape.
    None,
    /// Committed copies exist and snapshot this pool id.
    Pool(String),
    /// Committed copies exist but have no pool snapshot.
    UnknownPool,
}

impl CommittedCopyState {
    fn has_committed_copies(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Stable reason code for an init refusal, anomaly, rebuild, or force path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TapeInitError {
    /// A barcode is already assigned to a different physical UUID.
    BarcodeAssignedToDifferentUuid {
        /// UUID currently assigned to the barcode.
        assigned_uuid: TapeUuid,
        /// UUID found at BOT, when one exists.
        bot_uuid: Option<TapeUuid>,
    },
    /// The barcode has been retired.
    BarcodeRetired,
    /// The catalog row matched by barcode does not match the BOT UUID.
    MediaSwapReusedBarcode {
        /// UUID found at BOT.
        bot_uuid: TapeUuid,
        /// UUID from the catalog row.
        catalog_uuid: TapeUuid,
    },
    /// The barcode changed without an explicit relabel record.
    BarcodeChangedWithoutRelabel,
    /// A read error made the BOT state unknowable.
    BotReadError,
    /// A known non-Remanence format was found at BOT.
    ForeignFormat(String),
    /// Readable BOT bytes did not contain a known signature.
    UnrecognizedData,
    /// A Remanence bootstrap UUID had no catalog row.
    MissingCatalogRow {
        /// UUID found at BOT.
        bot_uuid: TapeUuid,
    },
    /// Physical data exists after the bootstrap.
    PhysicalDataPastBootstrap,
    /// Committed copies reference this tape.
    CommittedCopiesPresent,
    /// Committed copy pool snapshot conflicts with the derived pool.
    TapePoolAssignmentConflict {
        /// Pool snapshot on committed copies; `None` means unknown/unassigned.
        committed_pool: Option<String>,
        /// Pool derived from the physical barcode.
        derived_pool: String,
    },
    /// Bootstrap and catalog geometry differ but the tape appears unwritten.
    GeometryMismatch,
    /// The catalog says the tape is not unwritten, even though no copy rows
    /// were projected into the decision input.
    CatalogIndicatesWritten,
}

/// Safe init outcome for one projected tape state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitDecision {
    /// Safe to write a new bootstrap and provision a new catalog row.
    FreshInit,
    /// Existing bootstrap/catalog state is already the desired unwritten tape.
    IdempotentNoOp,
    /// Scoped `--force` may re-provision the clean Remanence tape.
    RequireForce {
        /// Stable reason code for the force path.
        reason: TapeInitError,
    },
    /// Refuse because proceeding would clobber or risk data.
    RefuseClobber {
        /// Stable reason code for the refusal.
        reason: TapeInitError,
    },
    /// Refuse because identity or pool state is anomalous.
    Anomaly {
        /// Stable reason code for the anomaly.
        reason: TapeInitError,
    },
    /// A Remanence bootstrap exists without catalog state; rebuild is explicit.
    NeedsExplicitRebuild {
        /// Stable reason code for the rebuild path.
        reason: TapeInitError,
    },
}

/// Operator write permissions for acting on an init decision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TapeInitWriteOptions {
    /// Run the whole gauntlet but write nothing.
    pub dry_run: bool,
    /// Scoped force for `InitDecision::RequireForce` only.
    pub force: bool,
    /// Separate confirmed override for data-clobber refusals.
    pub clobber_data_confirmed: bool,
}

/// Result of attempting to apply the BOT write portion of an init decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapeInitWriteAction {
    /// A bootstrap was written.
    WroteBootstrap,
    /// The decision would write, but dry-run suppressed the write.
    DryRunWouldWrite,
    /// The decision is an idempotent no-op and writes nothing.
    IdempotentNoOp,
    /// The decision was not writable with the provided flags.
    Refused,
}

/// Errors from applying the init write gate.
#[derive(Debug, thiserror::Error)]
pub enum TapeInitWriteError {
    /// `--clobber-data` is never meaningful during dry-run.
    #[error("--clobber-data is rejected in --dry-run")]
    ClobberDataInDryRun,
    /// Lower-level bootstrap write failed.
    #[error(transparent)]
    Write(#[from] PoolWriteError),
    /// Timestamp formatting failed.
    #[error(transparent)]
    TimeFormat(#[from] time::error::Format),
}

/// Decide whether `rem tape init` may write, must no-op, or must refuse.
///
/// This function encodes the safety gauntlet for already-projected facts:
/// blank is only BLANK CHECK/EOD, read errors fail closed, readable foreign or
/// unrecognized data refuses, and committed-copy pool conflicts are anomalies.
pub fn decide_tape_init(
    bot: &BotClassification,
    catalog_row: Option<&CatalogTapeInitRow>,
    barcode_state: &BarcodeLifecycleState,
    derived_pool: &str,
    physical_data_past_bootstrap: bool,
    committed_copies: &CommittedCopyState,
) -> InitDecision {
    if let Some(reason) = committed_pool_conflict(committed_copies, derived_pool) {
        return InitDecision::Anomaly { reason };
    }

    match bot {
        BotClassification::ReadError => refuse(TapeInitError::BotReadError),
        BotClassification::ForeignFormat { name } => {
            refuse(TapeInitError::ForeignFormat(name.clone()))
        }
        BotClassification::UnrecognizedData => refuse(TapeInitError::UnrecognizedData),
        BotClassification::BlankCheckEod => decide_blank_init(barcode_state, committed_copies),
        BotClassification::OursBootstrap { uuid, geometry } => decide_ours_init(
            *uuid,
            geometry,
            catalog_row,
            barcode_state,
            physical_data_past_bootstrap,
            committed_copies,
        ),
    }
}

/// Known format ids for BOT sniffing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatId {
    /// Remanence bootstrap tape file.
    RemanenceBootstrap,
    /// Legacy Remanence BRU archive format.
    LegacyBru,
    /// Legacy tar archive format.
    LegacyTar,
}

impl FormatId {
    /// Stable human-readable format id.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RemanenceBootstrap => "remanence-bootstrap",
            Self::LegacyBru => "remanence-bru",
            Self::LegacyTar => "legacy-tar",
        }
    }
}

/// Sniff a BOT block for a known Remanence or legacy format signature.
///
/// The legacy BRU/tar hooks intentionally return `None` until their format
/// crates expose byte-slice sniffers; they are still represented here so the
/// registry shape is explicit.
pub fn sniff(bot_bytes: &[u8]) -> Option<FormatId> {
    sniff_remanence_bootstrap(bot_bytes)
        .or_else(|| sniff_legacy_bru(bot_bytes))
        .or_else(|| sniff_legacy_tar(bot_bytes))
}

/// Classify readable BOT bytes for init. Blank and read-error states must come
/// from the drive path, not from this byte-slice helper.
pub fn classify_bot_bytes(bot_bytes: &[u8]) -> BotClassification {
    if let Ok(payload) = parse_bootstrap_block(bot_bytes) {
        return BotClassification::OursBootstrap {
            uuid: payload.tape_uuid,
            geometry: geometry_from_bootstrap(&payload),
        };
    }

    match sniff(bot_bytes) {
        Some(format) => BotClassification::ForeignFormat {
            name: format.as_str().to_string(),
        },
        None => BotClassification::UnrecognizedData,
    }
}

/// Project Layer 4 catalog state into the reviewed tape-init decision inputs.
///
/// Retired-barcode persistence is not present in the current catalog schema, so
/// this projects barcodes as available/assigned and records a note instead of
/// pretending a retired lookup was performed.
pub fn project_tape_init_catalog_inputs(
    catalog: &CatalogIndex,
    voltag: &str,
    bot: &BotClassification,
    derived_pool: &str,
) -> Result<TapeInitCatalogProjection, StateError> {
    let mut notes = vec![
        "retired barcode persistence is deferred; no retired barcode row was projected".to_string(),
    ];
    let barcode_row = catalog.get_tape_by_voltag(voltag)?;
    let barcode_state = match barcode_row.as_ref() {
        Some(row) => BarcodeLifecycleState::AssignedTo(tape_uuid_from_record(row)?),
        None => BarcodeLifecycleState::Available,
    };

    let bot_uuid = match bot {
        BotClassification::OursBootstrap { uuid, .. } => Some(*uuid),
        _ => None,
    };
    let catalog_row = match bot_uuid {
        Some(uuid) => catalog
            .get_tape(uuid.as_slice())?
            .map(|row| catalog_init_row_from_tape_record(&row, voltag))
            .transpose()?,
        None => None,
    };

    if matches!(
        catalog_row.as_ref().map(|row| &row.barcode_relation),
        Some(CatalogBarcodeRelation::DiffersWithoutRelabel)
    ) {
        notes.push(
            "barcode relabel persistence is deferred; catalog/physical barcode mismatch is treated as unrecorded"
                .to_string(),
        );
    }

    let committed_uuid = bot_uuid.or_else(|| {
        barcode_row
            .as_ref()
            .and_then(|row| tape_uuid_from_record(row).ok())
    });
    let committed_copies = match committed_uuid {
        Some(uuid) => committed_copy_state_for_tape(catalog, uuid.as_slice(), derived_pool)?,
        None => CommittedCopyState::None,
    };

    Ok(TapeInitCatalogProjection {
        catalog_row,
        barcode_state,
        committed_copies,
        notes,
    })
}

/// Read BOT through a [`remanence_library::BlockSource`] and project the exact
/// physical facts consumed by `decide_tape_init`.
///
/// Blank is recognized only from SCSI BLANK CHECK/EOD on the BOT read. Any
/// readable bytes, including an all-zero block, are non-blank and are passed to
/// the format sniffers.
pub fn classify_bot_from_source(
    source: &mut dyn remanence_library::BlockSource,
) -> BotInitProjection {
    if source.locate(0).is_err() {
        return BotInitProjection {
            classification: BotClassification::ReadError,
            physical_data_past_bootstrap: false,
        };
    }

    let mut block = vec![0u8; BOT_CLASSIFY_READ_BYTES];
    let classification = match source.read_block(&mut block) {
        Ok(read) => classify_bot_bytes(&block[..read]),
        Err(err) if is_blank_check_eod(&err) => BotClassification::BlankCheckEod,
        Err(_) => BotClassification::ReadError,
    };

    let physical_data_past_bootstrap =
        matches!(classification, BotClassification::OursBootstrap { .. })
            && data_exists_at_current_position(source);

    BotInitProjection {
        classification,
        physical_data_past_bootstrap,
    }
}

/// Apply only the BOT bootstrap write gate for an init decision.
///
/// This helper deliberately knows nothing about drive loading or catalog
/// projection; it is the narrow safety guard that keeps non-writable decisions
/// from reaching `write_tape_bootstrap`.
pub fn maybe_write_tape_init_bootstrap(
    sink: &mut dyn remanence_library::BlockSink,
    decision: &InitDecision,
    options: TapeInitWriteOptions,
    tape_uuid: TapeUuid,
    block_size: u32,
    parity: ParityConfig,
    written_by_version: &str,
) -> Result<TapeInitWriteAction, TapeInitWriteError> {
    if options.dry_run && options.clobber_data_confirmed {
        return Err(TapeInitWriteError::ClobberDataInDryRun);
    }

    let may_write = match decision {
        InitDecision::FreshInit => true,
        InitDecision::RequireForce { .. } => options.force,
        InitDecision::RefuseClobber {
            reason: TapeInitError::BotReadError,
        } => false,
        InitDecision::RefuseClobber { .. } => options.clobber_data_confirmed,
        InitDecision::IdempotentNoOp
        | InitDecision::Anomaly { .. }
        | InitDecision::NeedsExplicitRebuild { .. } => false,
    };

    if !may_write {
        return Ok(match decision {
            InitDecision::IdempotentNoOp => TapeInitWriteAction::IdempotentNoOp,
            _ => TapeInitWriteAction::Refused,
        });
    }
    if options.dry_run {
        return Ok(TapeInitWriteAction::DryRunWouldWrite);
    }

    let payload = build_tape_bootstrap(
        tape_uuid,
        block_size,
        parity,
        time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339)?,
        written_by_version,
    );
    write_tape_bootstrap(sink, &payload)?;
    Ok(TapeInitWriteAction::WroteBootstrap)
}

fn decide_blank_init(
    barcode_state: &BarcodeLifecycleState,
    committed_copies: &CommittedCopyState,
) -> InitDecision {
    match barcode_state {
        BarcodeLifecycleState::Available => {}
        BarcodeLifecycleState::AssignedTo(assigned_uuid) => {
            return InitDecision::Anomaly {
                reason: TapeInitError::BarcodeAssignedToDifferentUuid {
                    assigned_uuid: *assigned_uuid,
                    bot_uuid: None,
                },
            };
        }
        BarcodeLifecycleState::Retired => {
            return InitDecision::Anomaly {
                reason: TapeInitError::BarcodeRetired,
            };
        }
    }
    if committed_copies.has_committed_copies() {
        return refuse(TapeInitError::CommittedCopiesPresent);
    }
    InitDecision::FreshInit
}

fn decide_ours_init(
    bot_uuid: TapeUuid,
    bot_geometry: &TapeInitGeometry,
    catalog_row: Option<&CatalogTapeInitRow>,
    barcode_state: &BarcodeLifecycleState,
    physical_data_past_bootstrap: bool,
    committed_copies: &CommittedCopyState,
) -> InitDecision {
    match barcode_state {
        BarcodeLifecycleState::Available => {}
        BarcodeLifecycleState::AssignedTo(assigned_uuid) if *assigned_uuid == bot_uuid => {}
        BarcodeLifecycleState::AssignedTo(assigned_uuid) => {
            return InitDecision::Anomaly {
                reason: TapeInitError::BarcodeAssignedToDifferentUuid {
                    assigned_uuid: *assigned_uuid,
                    bot_uuid: Some(bot_uuid),
                },
            };
        }
        BarcodeLifecycleState::Retired => {
            return InitDecision::Anomaly {
                reason: TapeInitError::BarcodeRetired,
            };
        }
    }

    let Some(catalog_row) = catalog_row else {
        return InitDecision::NeedsExplicitRebuild {
            reason: TapeInitError::MissingCatalogRow { bot_uuid },
        };
    };

    if catalog_row.uuid != bot_uuid {
        return InitDecision::Anomaly {
            reason: TapeInitError::MediaSwapReusedBarcode {
                bot_uuid,
                catalog_uuid: catalog_row.uuid,
            },
        };
    }
    // A retired identity's medium is free to re-init under a fresh uuid: the
    // retire ceremony already declared the data on it dead and detached the
    // barcode. The data-past-bootstrap probe is deliberately skipped here —
    // demanding `CLOBBER` again would be double ceremony for the same intent
    // the operator acknowledged at retire time. This arm is only reachable
    // when the barcode is free (or points at this uuid); a barcode owned by
    // a different live identity already returned `Anomaly` above. The
    // retired row itself is never reused: `provision_tape` refuses it,
    // `force` included, so `FreshInit` provisions a brand-new row. Returning
    // before the barcode-relation check is also deliberate — a retired row's
    // detached (NULL) voltag is an artifact of retirement, not an unrecorded
    // relabel.
    if catalog_row.disposition == CatalogRowDisposition::Retired {
        return InitDecision::FreshInit;
    }
    if catalog_row.barcode_relation == CatalogBarcodeRelation::DiffersWithoutRelabel {
        return InitDecision::Anomaly {
            reason: TapeInitError::BarcodeChangedWithoutRelabel,
        };
    }
    if physical_data_past_bootstrap {
        return refuse(TapeInitError::PhysicalDataPastBootstrap);
    }
    if committed_copies.has_committed_copies() {
        return refuse(TapeInitError::CommittedCopiesPresent);
    }
    if bot_geometry != &catalog_row.geometry {
        return InitDecision::RequireForce {
            reason: TapeInitError::GeometryMismatch,
        };
    }
    if !catalog_row.catalog_unwritten {
        return refuse(TapeInitError::CatalogIndicatesWritten);
    }
    InitDecision::IdempotentNoOp
}

fn committed_pool_conflict(
    committed_copies: &CommittedCopyState,
    derived_pool: &str,
) -> Option<TapeInitError> {
    match committed_copies {
        CommittedCopyState::None => None,
        CommittedCopyState::Pool(pool_id) if pool_id == derived_pool => None,
        CommittedCopyState::Pool(pool_id) => Some(TapeInitError::TapePoolAssignmentConflict {
            committed_pool: Some(pool_id.clone()),
            derived_pool: derived_pool.to_string(),
        }),
        CommittedCopyState::UnknownPool => Some(TapeInitError::TapePoolAssignmentConflict {
            committed_pool: None,
            derived_pool: derived_pool.to_string(),
        }),
    }
}

fn refuse(reason: TapeInitError) -> InitDecision {
    InitDecision::RefuseClobber { reason }
}

fn sniff_remanence_bootstrap(bot_bytes: &[u8]) -> Option<FormatId> {
    parse_bootstrap_block(bot_bytes)
        .ok()
        .map(|_| FormatId::RemanenceBootstrap)
}

fn sniff_legacy_bru(_bot_bytes: &[u8]) -> Option<FormatId> {
    None
}

fn sniff_legacy_tar(_bot_bytes: &[u8]) -> Option<FormatId> {
    None
}

fn geometry_from_bootstrap(payload: &BootstrapPayload) -> TapeInitGeometry {
    TapeInitGeometry {
        block_size_bytes: payload.block_size_bytes,
        parity: parity_config_from_bootstrap(payload),
    }
}

fn catalog_init_row_from_tape_record(
    row: &TapeRecord,
    physical_voltag: &str,
) -> Result<CatalogTapeInitRow, StateError> {
    let barcode_relation = match row.voltag.as_deref() {
        Some(voltag) if voltag == physical_voltag => CatalogBarcodeRelation::Matches,
        Some(_) => CatalogBarcodeRelation::DiffersWithoutRelabel,
        None => CatalogBarcodeRelation::DiffersWithoutRelabel,
    };
    let disposition = if row.state == "retired" {
        CatalogRowDisposition::Retired
    } else {
        CatalogRowDisposition::Active
    };
    Ok(CatalogTapeInitRow {
        uuid: tape_uuid_from_record(row)?,
        geometry: geometry_from_tape_record(row)?,
        catalog_unwritten: row.last_committed_tape_file.is_none(),
        barcode_relation,
        disposition,
    })
}

fn tape_uuid_from_record(row: &TapeRecord) -> Result<TapeUuid, StateError> {
    row.tape_uuid.as_slice().try_into().map_err(|_| {
        StateError::IndexCorrupt(format!(
            "catalog tape row has {} UUID bytes, expected 16",
            row.tape_uuid.len()
        ))
    })
}

fn geometry_from_tape_record(row: &TapeRecord) -> Result<TapeInitGeometry, StateError> {
    let block_size = row.block_size.ok_or_else(|| {
        StateError::IndexCorrupt(format!(
            "catalog tape {} is missing block_size",
            uuid_display(row.tape_uuid.as_slice())
        ))
    })?;
    let block_size_bytes = u32::try_from(block_size).map_err(|_| {
        StateError::IndexCorrupt(format!(
            "catalog tape {} block_size {block_size} exceeds u32",
            uuid_display(row.tape_uuid.as_slice())
        ))
    })?;
    let parity = match row.scheme_id.as_deref() {
        Some(scheme_id) => ParityConfig::Scheme(ParityScheme {
            id: SchemeId::new_owned(scheme_id.to_string()),
            data_blocks_per_stripe: parity_u16_field(
                row.data_blocks_per_stripe,
                "data_blocks_per_stripe",
                row,
            )?,
            parity_blocks_per_stripe: parity_u16_field(
                row.parity_blocks_per_stripe,
                "parity_blocks_per_stripe",
                row,
            )?,
            stripes_per_neighborhood: row.stripes_per_neighborhood.ok_or_else(|| {
                StateError::IndexCorrupt(format!(
                    "catalog tape {} parity scheme is missing stripes_per_neighborhood",
                    uuid_display(row.tape_uuid.as_slice())
                ))
            })?,
        }),
        None => ParityConfig::None,
    };
    Ok(TapeInitGeometry {
        block_size_bytes,
        parity,
    })
}

fn parity_u16_field(value: Option<u32>, field: &str, row: &TapeRecord) -> Result<u16, StateError> {
    let value = value.ok_or_else(|| {
        StateError::IndexCorrupt(format!(
            "catalog tape {} parity scheme is missing {field}",
            uuid_display(row.tape_uuid.as_slice())
        ))
    })?;
    u16::try_from(value).map_err(|_| {
        StateError::IndexCorrupt(format!(
            "catalog tape {} parity field {field}={value} exceeds u16",
            uuid_display(row.tape_uuid.as_slice())
        ))
    })
}

fn committed_copy_state_for_tape(
    catalog: &CatalogIndex,
    tape_uuid: &[u8],
    derived_pool: &str,
) -> Result<CommittedCopyState, StateError> {
    let pools = catalog.committed_copy_pool_snapshots(tape_uuid)?;
    if pools.is_empty() {
        return Ok(CommittedCopyState::None);
    }
    if pools.iter().any(Option::is_none) {
        return Ok(CommittedCopyState::UnknownPool);
    }
    let first_pool = pools
        .iter()
        .flatten()
        .find(|pool| pool.as_str() != derived_pool)
        .or_else(|| pools.iter().flatten().next())
        .expect("non-empty committed pool list");
    Ok(CommittedCopyState::Pool(first_pool.clone()))
}

fn uuid_display(bytes: &[u8]) -> String {
    match <[u8; 16]>::try_from(bytes) {
        Ok(uuid) => uuid::Uuid::from_bytes(uuid).to_string(),
        Err(_) => format!("{} bytes", bytes.len()),
    }
}

fn data_exists_at_current_position(source: &mut dyn remanence_library::BlockSource) -> bool {
    let mut next = vec![0u8; BOT_CLASSIFY_READ_BYTES];
    match source.read_block(&mut next) {
        Ok(_) => true,
        Err(err) if is_blank_check_eod(&err) => false,
        Err(_) => true,
    }
}

fn is_blank_check_eod(err: &remanence_library::TapeIoError) -> bool {
    sense_key_asc(err) == Some((0x08, 0x00, 0x05))
}

fn sense_key_asc(err: &remanence_library::TapeIoError) -> Option<(u8, u8, u8)> {
    match err {
        remanence_library::TapeIoError::CheckCondition(
            remanence_library::scsi::ScsiError::CheckCondition { sense, .. },
        ) => {
            let response_code = sense.first()? & 0x7F;
            if response_code != 0x70 && response_code != 0x71 {
                return None;
            }
            Some((
                sense.get(2)? & 0x0F,
                sense.get(12).copied().unwrap_or(0),
                sense.get(13).copied().unwrap_or(0),
            ))
        }
        _ => None,
    }
}

fn parity_config_from_bootstrap(payload: &BootstrapPayload) -> ParityConfig {
    if payload.no_parity_flag {
        return ParityConfig::None;
    }
    match &payload.scheme {
        Some(scheme) => ParityConfig::Scheme(ParityScheme {
            id: SchemeId::new_owned(scheme.id.clone()),
            data_blocks_per_stripe: scheme.data_blocks_per_stripe,
            parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
            stripes_per_neighborhood: scheme.stripes_per_neighborhood,
        }),
        None => ParityConfig::None,
    }
}

#[cfg(test)]
mod tests {
    use remanence_library::{VecBlockSink, VecBlockSource};
    use remanence_parity::bootstrap::write_bootstrap_block;

    use super::*;

    const BOT_UUID: TapeUuid = [1; 16];
    const OTHER_UUID: TapeUuid = [2; 16];
    const POOL_A: &str = "camera.copy-a";
    const POOL_B: &str = "camera.copy-b";

    fn geometry() -> TapeInitGeometry {
        TapeInitGeometry {
            block_size_bytes: 4096,
            parity: ParityConfig::None,
        }
    }

    fn other_geometry() -> TapeInitGeometry {
        TapeInitGeometry {
            block_size_bytes: 8192,
            parity: ParityConfig::None,
        }
    }

    fn ours_bot() -> BotClassification {
        BotClassification::OursBootstrap {
            uuid: BOT_UUID,
            geometry: geometry(),
        }
    }

    fn catalog_row() -> CatalogTapeInitRow {
        CatalogTapeInitRow {
            uuid: BOT_UUID,
            geometry: geometry(),
            catalog_unwritten: true,
            barcode_relation: CatalogBarcodeRelation::Matches,
            disposition: CatalogRowDisposition::Active,
        }
    }

    fn retired_catalog_row() -> CatalogTapeInitRow {
        CatalogTapeInitRow {
            uuid: BOT_UUID,
            geometry: geometry(),
            // A retired row keeps its committed history and has no voltag.
            catalog_unwritten: false,
            barcode_relation: CatalogBarcodeRelation::DiffersWithoutRelabel,
            disposition: CatalogRowDisposition::Retired,
        }
    }

    fn decide(
        bot: &BotClassification,
        catalog_row: Option<&CatalogTapeInitRow>,
        barcode_state: &BarcodeLifecycleState,
        physical_data_past_bootstrap: bool,
        committed_copies: &CommittedCopyState,
    ) -> InitDecision {
        decide_tape_init(
            bot,
            catalog_row,
            barcode_state,
            POOL_A,
            physical_data_past_bootstrap,
            committed_copies,
        )
    }

    #[test]
    fn blank_check_available_barcode_is_fresh_init() {
        assert_eq!(
            decide(
                &BotClassification::BlankCheckEod,
                None,
                &BarcodeLifecycleState::Available,
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::FreshInit
        );
    }

    #[test]
    fn blank_check_assigned_barcode_is_media_swap_anomaly() {
        assert_eq!(
            decide(
                &BotClassification::BlankCheckEod,
                None,
                &BarcodeLifecycleState::AssignedTo(OTHER_UUID),
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::BarcodeAssignedToDifferentUuid {
                    assigned_uuid: OTHER_UUID,
                    bot_uuid: None,
                }
            }
        );
    }

    #[test]
    fn retired_barcode_is_anomaly() {
        assert_eq!(
            decide(
                &BotClassification::BlankCheckEod,
                None,
                &BarcodeLifecycleState::Retired,
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::BarcodeRetired
            }
        );
    }

    #[test]
    fn read_error_refuses_fail_closed() {
        assert_eq!(
            decide(
                &BotClassification::ReadError,
                None,
                &BarcodeLifecycleState::Available,
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::BotReadError
            }
        );
    }

    #[test]
    fn foreign_format_refuses() {
        assert_eq!(
            decide(
                &BotClassification::ForeignFormat {
                    name: "legacy-tar".to_string()
                },
                None,
                &BarcodeLifecycleState::Available,
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::ForeignFormat("legacy-tar".to_string())
            }
        );
    }

    #[test]
    fn unrecognized_readable_data_refuses() {
        assert_eq!(
            decide(
                &BotClassification::UnrecognizedData,
                None,
                &BarcodeLifecycleState::Available,
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::UnrecognizedData
            }
        );
    }

    #[test]
    fn matching_unwritten_bootstrap_is_idempotent_noop() {
        let row = catalog_row();

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::IdempotentNoOp
        );
    }

    #[test]
    fn relabeled_matching_uuid_can_noop_when_relabel_record_exists() {
        let mut row = catalog_row();
        row.barcode_relation = CatalogBarcodeRelation::DiffersWithRecordedRelabel;

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::IdempotentNoOp
        );
    }

    #[test]
    fn barcode_change_without_relabel_is_anomaly() {
        let mut row = catalog_row();
        row.barcode_relation = CatalogBarcodeRelation::DiffersWithoutRelabel;

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::BarcodeChangedWithoutRelabel
            }
        );
    }

    #[test]
    fn physical_data_past_bootstrap_refuses_even_without_committed_copies() {
        let row = catalog_row();

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                true,
                &CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::PhysicalDataPastBootstrap
            }
        );
    }

    #[test]
    fn retired_row_with_available_barcode_is_fresh_init_despite_physical_data() {
        let row = retired_catalog_row();

        // The retire ceremony already declared the data dead, so the
        // physical-data-past-bootstrap probe must not demand CLOBBER again.
        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::Available,
                true,
                &CommittedCopyState::None,
            ),
            InitDecision::FreshInit
        );
    }

    #[test]
    fn retired_row_with_barcode_assigned_elsewhere_stays_anomaly() {
        let row = retired_catalog_row();

        // The barcode belongs to a live identity; retirement of the BOT
        // identity does not loosen the barcode-ownership gauntlet.
        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(OTHER_UUID),
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::BarcodeAssignedToDifferentUuid {
                    assigned_uuid: OTHER_UUID,
                    bot_uuid: Some(BOT_UUID),
                }
            }
        );
    }

    #[test]
    fn foreign_ours_bot_without_row_keeps_missing_catalog_row_mapping() {
        // Retire must not whitelist foreign tapes: a rem bootstrap the
        // catalog has never seen still maps to the guarded rebuild path.
        assert_eq!(
            decide(
                &ours_bot(),
                None,
                &BarcodeLifecycleState::Available,
                true,
                &CommittedCopyState::None,
            ),
            InitDecision::NeedsExplicitRebuild {
                reason: TapeInitError::MissingCatalogRow { bot_uuid: BOT_UUID }
            }
        );
    }

    #[test]
    fn ours_without_catalog_row_needs_explicit_rebuild() {
        assert_eq!(
            decide(
                &ours_bot(),
                None,
                &BarcodeLifecycleState::Available,
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::NeedsExplicitRebuild {
                reason: TapeInitError::MissingCatalogRow { bot_uuid: BOT_UUID }
            }
        );
    }

    #[test]
    fn clean_geometry_mismatch_requires_scoped_force() {
        let mut row = catalog_row();
        row.geometry = other_geometry();

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::RequireForce {
                reason: TapeInitError::GeometryMismatch
            }
        );
    }

    #[test]
    fn committed_copies_refuse_plain_init() {
        let row = catalog_row();

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                &CommittedCopyState::Pool(POOL_A.to_string()),
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::CommittedCopiesPresent
            }
        );
    }

    #[test]
    fn committed_copies_in_foreign_pool_are_assignment_conflict_anomaly() {
        let row = catalog_row();

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                &CommittedCopyState::Pool(POOL_B.to_string()),
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::TapePoolAssignmentConflict {
                    committed_pool: Some(POOL_B.to_string()),
                    derived_pool: POOL_A.to_string(),
                }
            }
        );
    }

    #[test]
    fn committed_copies_with_unknown_pool_are_assignment_conflict_anomaly() {
        let row = catalog_row();

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                &CommittedCopyState::UnknownPool,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::TapePoolAssignmentConflict {
                    committed_pool: None,
                    derived_pool: POOL_A.to_string(),
                }
            }
        );
    }

    #[test]
    fn assigned_barcode_for_different_uuid_is_anomaly() {
        let row = catalog_row();

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(OTHER_UUID),
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::BarcodeAssignedToDifferentUuid {
                    assigned_uuid: OTHER_UUID,
                    bot_uuid: Some(BOT_UUID),
                }
            }
        );
    }

    #[test]
    fn barcode_matched_catalog_row_with_different_bot_uuid_is_anomaly() {
        let mut row = catalog_row();
        row.uuid = OTHER_UUID;

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::Available,
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::Anomaly {
                reason: TapeInitError::MediaSwapReusedBarcode {
                    bot_uuid: BOT_UUID,
                    catalog_uuid: OTHER_UUID,
                }
            }
        );
    }

    #[test]
    fn catalog_written_row_refuses_without_force_path() {
        let mut row = catalog_row();
        row.catalog_unwritten = false;

        assert_eq!(
            decide(
                &ours_bot(),
                Some(&row),
                &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                false,
                &CommittedCopyState::None,
            ),
            InitDecision::RefuseClobber {
                reason: TapeInitError::CatalogIndicatesWritten
            }
        );
    }

    #[test]
    fn sniff_detects_remanence_bootstrap_and_classifies_geometry() {
        let payload = build_tape_bootstrap(
            BOT_UUID,
            4096,
            ParityConfig::None,
            "2026-05-30T00:00:00Z",
            "test",
        );
        let mut block = vec![0; payload.block_size_bytes as usize];
        write_bootstrap_block(&payload, &mut block).expect("encode bootstrap");

        assert_eq!(sniff(&block), Some(FormatId::RemanenceBootstrap));
        assert_eq!(
            classify_bot_bytes(&block),
            BotClassification::OursBootstrap {
                uuid: BOT_UUID,
                geometry: geometry(),
            }
        );
    }

    #[test]
    fn zero_bytes_are_unrecognized_data_not_blank() {
        assert_eq!(
            sniff(&[0; 4096]),
            None,
            "zeros must not be treated as blank or Remanence"
        );
        assert_eq!(
            classify_bot_bytes(&[0; 4096]),
            BotClassification::UnrecognizedData
        );
    }

    #[test]
    fn bot_projection_blank_only_from_blank_check_eod() {
        let mut source = VecBlockSource::new(Vec::new());

        assert_eq!(
            classify_bot_from_source(&mut source),
            BotInitProjection {
                classification: BotClassification::BlankCheckEod,
                physical_data_past_bootstrap: false,
            }
        );
    }

    #[test]
    fn bot_projection_zero_block_is_unrecognized_data() {
        let mut source = VecBlockSource::new(vec![vec![0u8; 4096]]);

        assert_eq!(
            classify_bot_from_source(&mut source),
            BotInitProjection {
                classification: BotClassification::UnrecognizedData,
                physical_data_past_bootstrap: false,
            }
        );
    }

    #[test]
    fn bot_projection_ours_bootstrap_detects_data_after_bootstrap() {
        let payload = build_tape_bootstrap(
            BOT_UUID,
            4096,
            ParityConfig::None,
            "2026-05-30T00:00:00Z",
            "test",
        );
        let mut block = vec![0; payload.block_size_bytes as usize];
        write_bootstrap_block(&payload, &mut block).expect("encode bootstrap");
        let mut source = VecBlockSource::new(vec![block, vec![0xA5; 4096]]);

        assert_eq!(
            classify_bot_from_source(&mut source),
            BotInitProjection {
                classification: ours_bot(),
                physical_data_past_bootstrap: true,
            }
        );
    }

    #[test]
    fn bot_projection_ours_bootstrap_without_following_data_is_clean() {
        let payload = build_tape_bootstrap(
            BOT_UUID,
            4096,
            ParityConfig::None,
            "2026-05-30T00:00:00Z",
            "test",
        );
        let mut block = vec![0; payload.block_size_bytes as usize];
        write_bootstrap_block(&payload, &mut block).expect("encode bootstrap");
        let mut source = VecBlockSource::new(vec![block]);

        assert_eq!(
            classify_bot_from_source(&mut source),
            BotInitProjection {
                classification: ours_bot(),
                physical_data_past_bootstrap: false,
            }
        );
    }

    #[test]
    fn unsafe_decisions_never_write_bootstrap_without_clobber_or_force() {
        let cases = [
            decide(
                &BotClassification::ForeignFormat {
                    name: "legacy-tar".to_string(),
                },
                None,
                &BarcodeLifecycleState::Available,
                false,
                &CommittedCopyState::None,
            ),
            decide(
                &BotClassification::UnrecognizedData,
                None,
                &BarcodeLifecycleState::Available,
                false,
                &CommittedCopyState::None,
            ),
            decide(
                &BotClassification::ReadError,
                None,
                &BarcodeLifecycleState::Available,
                false,
                &CommittedCopyState::None,
            ),
            {
                let row = catalog_row();
                decide(
                    &ours_bot(),
                    Some(&row),
                    &BarcodeLifecycleState::AssignedTo(BOT_UUID),
                    true,
                    &CommittedCopyState::None,
                )
            },
        ];

        for decision in cases {
            let mut sink = VecBlockSink::new();
            let action = maybe_write_tape_init_bootstrap(
                &mut sink,
                &decision,
                TapeInitWriteOptions::default(),
                BOT_UUID,
                4096,
                ParityConfig::None,
                "test",
            )
            .expect("write gate returns");
            assert_eq!(action, TapeInitWriteAction::Refused);
            assert_eq!(sink.blocks.len(), 0, "decision {decision:?} wrote BOT");
            assert_eq!(sink.filemarks.len(), 0, "decision {decision:?} wrote FM");
        }
    }

    #[test]
    fn clobber_data_does_not_override_bot_read_error() {
        let decision = decide(
            &BotClassification::ReadError,
            None,
            &BarcodeLifecycleState::Available,
            false,
            &CommittedCopyState::None,
        );
        let mut sink = VecBlockSink::new();

        let action = maybe_write_tape_init_bootstrap(
            &mut sink,
            &decision,
            TapeInitWriteOptions {
                dry_run: false,
                force: false,
                clobber_data_confirmed: true,
            },
            BOT_UUID,
            4096,
            ParityConfig::None,
            "test",
        )
        .expect("write gate returns");

        assert_eq!(action, TapeInitWriteAction::Refused);
        assert!(sink.blocks.is_empty());
        assert!(sink.filemarks.is_empty());
    }

    #[test]
    fn fresh_init_reaches_bootstrap_write() {
        let mut sink = VecBlockSink::new();
        let action = maybe_write_tape_init_bootstrap(
            &mut sink,
            &InitDecision::FreshInit,
            TapeInitWriteOptions::default(),
            BOT_UUID,
            4096,
            ParityConfig::None,
            "test",
        )
        .expect("fresh write");

        assert_eq!(action, TapeInitWriteAction::WroteBootstrap);
        assert_eq!(sink.blocks.len(), 1);
        assert_eq!(sink.filemarks, vec![1]);
    }

    #[test]
    fn dry_run_suppresses_fresh_init_write() {
        let mut sink = VecBlockSink::new();
        let action = maybe_write_tape_init_bootstrap(
            &mut sink,
            &InitDecision::FreshInit,
            TapeInitWriteOptions {
                dry_run: true,
                force: false,
                clobber_data_confirmed: false,
            },
            BOT_UUID,
            4096,
            ParityConfig::None,
            "test",
        )
        .expect("dry-run write gate");

        assert_eq!(action, TapeInitWriteAction::DryRunWouldWrite);
        assert!(sink.blocks.is_empty());
        assert!(sink.filemarks.is_empty());
    }
}
