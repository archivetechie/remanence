//! Catalog-less terminal-index recovery reporting for `rem-debug tape`.
//!
//! This module composes the sole-BOT structural scan with terminal-index
//! selection and bounded plaintext-manifest/keyless REM-ENCRYPT checks. It
//! never consults the catalog, daemon, or host persistence.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use remanence_aead::{KeyFrame, RemObjectHeader, REM_OBJECT_FOOTER, REM_OBJECT_HEADER_LEN};
use remanence_parity::{
    bootstrap::discover_bootstrap_with_recovery_hints, read_terminal_index_inventory,
    scan_reconstruct_filemark_map_with_report_mode, FilemarkMap, ImageDirectoryRawSource,
    ObjectRecoveryRepresentation, ParityError, RawReadOutcome, RawTapeSource, ScanDamageKind,
    ScanDamagedRegion, ScanMode, ScanRecoveryHints, ScanTailTruncation, ScanTailTruncationKind,
    TapeFileKind, TapeFileMapEntry, TapeFilePosition, TapeIndexReplicaFileKind,
    TapeIndexReplicaMapEntry, TapeIndexReplicaObjectRow, TerminalInventoryOutcome,
};
use serde::{Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Out-of-band authority, kept separate from bootstrap evidence in the report.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct RecoveryHints {
    pub tape_uuid: String,
    pub block_size_bytes: u32,
    pub scheme: String,
}

/// Validate a supplied UUID while preserving its spelling for the report.
pub(crate) fn parse_tape_uuid(value: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(value).map_err(|error| error.to_string())?;
    Ok(value.to_string())
}

/// Accept only explicit parity geometry or an explicit absence of parity.
pub(crate) fn parse_scheme(value: &str) -> Result<String, String> {
    let argument = if value == "none" {
        value.to_string()
    } else {
        format!("custom:{value}")
    };
    remanence_parity::parse_parity_arg(&argument).map_err(|error| error.to_string())?;
    Ok(value.to_string())
}

/// Apply the existing tape block-size limits to recovery hints.
pub(crate) fn parse_block_size(value: &str) -> Result<u32, String> {
    u32::try_from(crate::parse_tape_block_size(value)?)
        .map_err(|_| "block size does not fit u32".to_string())
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CatalogLessRecoveryReport {
    report_version: u32,
    tape_uuid: Option<String>,
    block_size_bytes: Option<u32>,
    supplied: Option<RecoveryHints>,
    scan: RecoveryScanSummary,
    objects: Vec<RecoveryObjectReport>,
    totals: RecoveryTotals,
    success: bool,
}

impl CatalogLessRecoveryReport {
    fn has_failures(&self) -> bool {
        !self.success
    }
}

#[derive(Clone, Debug, Serialize)]
struct RecoveryScanSummary {
    #[serde(serialize_with = "serialize_optional_decimal_u64")]
    bootstrap_generation_used: Option<u64>,
    bootstrap_treated_as_unreadable: bool,
    identity_and_geometry_from_hints: bool,
    #[serde(serialize_with = "serialize_optional_decimal_u64")]
    bootstrap_tape_file_number: Option<u64>,
    overlay_source: &'static str,
    #[serde(serialize_with = "serialize_decimal_u64")]
    recovered_scope_tape_file_count: u64,
    damaged_regions: Vec<RecoveryDamageRegion>,
    truncation: Option<RecoveryTruncation>,
}

#[derive(Clone, Debug, Serialize)]
struct RecoveryDamageRegion {
    #[serde(serialize_with = "serialize_decimal_u64")]
    start_lba: u64,
    partition: u32,
    #[serde(serialize_with = "serialize_decimal_u64")]
    block_count: u64,
    kind: &'static str,
}

impl From<ScanDamagedRegion> for RecoveryDamageRegion {
    fn from(value: ScanDamagedRegion) -> Self {
        Self {
            start_lba: value.start.lba,
            partition: value.start.partition,
            block_count: value.block_count,
            kind: match value.kind {
                ScanDamageKind::UnreadableTapeFileHead => "unreadable_tape_file_head",
                ScanDamageKind::ClassificationCountMismatch => "classification_count_mismatch",
                ScanDamageKind::InvalidTerminalControl => "invalid_terminal_control",
            },
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct RecoveryTruncation {
    #[serde(serialize_with = "serialize_decimal_u64")]
    tape_file_number: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    start_lba: u64,
    partition: u32,
    kind: &'static str,
}

impl From<ScanTailTruncation> for RecoveryTruncation {
    fn from(value: ScanTailTruncation) -> Self {
        Self {
            tape_file_number: value.tape_file_number,
            start_lba: value.position.lba,
            partition: value.position.partition,
            kind: match value.kind {
                ScanTailTruncationKind::MissingTrailingFilemark => "missing_trailing_filemark",
                ScanTailTruncationKind::ZeroBlockFile => "zero_block_file",
                ScanTailTruncationKind::EmptyFile => "empty_file",
            },
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct RecoveryObjectReport {
    #[serde(serialize_with = "serialize_decimal_u64")]
    tape_file_number: u64,
    representation: &'static str,
    object_id: Value,
    object_id_encoding: &'static str,
    #[serde(skip)]
    object_id_human: String,
    #[serde(serialize_with = "serialize_decimal_u64")]
    stored_block_count: u64,
    map_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    map_detail: Option<String>,
    verification_status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    verification_detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct RecoveryTotals {
    #[serde(serialize_with = "serialize_decimal_u64")]
    objects_seen: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    map_agreeing: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    verified: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    failed: u64,
    #[serde(serialize_with = "serialize_decimal_u64")]
    beyond_scope: u64,
}

fn serialize_decimal_u64<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn serialize_optional_decimal_u64<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        Some(value) => serializer.serialize_some(&value.to_string()),
        None => serializer.serialize_none(),
    }
}

struct RecoveredMap {
    map: FilemarkMap,
    object_rows: Vec<TapeIndexReplicaObjectRow>,
    scope_tape_file_count: u64,
    bootstrap_generation_used: Option<u64>,
    overlay_source: &'static str,
    terminal_authority: bool,
    damaged_regions: Vec<ScanDamagedRegion>,
    truncation: Option<ScanTailTruncation>,
}

/// Run a report against a published-layout image directory.
pub(crate) fn run_image_recovery_report(
    image_directory: &Path,
    hints: Option<RecoveryHints>,
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let mut source = match ImageDirectoryRawSource::open(image_directory) {
        Ok(source) => source,
        Err(error) => {
            let _ = writeln!(err, "error: open recovery image: {error}");
            return ExitCode::from(1);
        }
    };
    let candidate_block_sizes = source.candidate_block_sizes().to_vec();
    run_raw_recovery_report(
        &mut source,
        &candidate_block_sizes,
        hints,
        json_output,
        out,
        err,
    )
}

/// Run a report against any production Layer 3c raw source.
pub(crate) fn run_raw_recovery_report(
    source: &mut dyn RawTapeSource,
    candidate_block_sizes: &[u32],
    hints: Option<RecoveryHints>,
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let report = match build_recovery_report(source, candidate_block_sizes, hints) {
        Ok(report) => report,
        Err(error) => {
            let _ = writeln!(err, "error: catalog-less recovery report: {error}");
            return ExitCode::from(1);
        }
    };
    let failed = report.has_failures();
    let rendered = if json_output {
        serde_json::to_writer_pretty(&mut *out, &report)
            .and_then(|()| writeln!(out).map_err(serde_json::Error::io))
            .map_err(|error| format!("write JSON recovery report: {error}"))
    } else {
        print_human_report(&report, out)
            .map_err(|error| format!("write human recovery report: {error}"))
    };
    if let Err(error) = rendered {
        let _ = writeln!(err, "error: {error}");
        return ExitCode::from(1);
    }
    if failed {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

fn build_recovery_report(
    source: &mut dyn RawTapeSource,
    candidate_block_sizes: &[u32],
    hints: Option<RecoveryHints>,
) -> Result<CatalogLessRecoveryReport, String> {
    let mut candidates = candidate_block_sizes.to_vec();
    let supplied_uuid = hints
        .as_ref()
        .map(|hints| {
            parse_scheme(&hints.scheme)?;
            parse_block_size(&hints.block_size_bytes.to_string())?;
            candidates.push(hints.block_size_bytes);
            uuid::Uuid::parse_str(&hints.tape_uuid)
                .map(|uuid| *uuid.as_bytes())
                .map_err(|error| format!("supplied tape UUID: {error}"))
        })
        .transpose()?;
    if candidates.is_empty() {
        return Err("no candidate fixed-block sizes were supplied".to_string());
    }
    let scan_hints = hints
        .as_ref()
        .map(|hints| {
            let scheme_arg = if hints.scheme == "none" {
                "none".to_string()
            } else {
                format!("custom:{}", hints.scheme)
            };
            Ok::<_, String>(ScanRecoveryHints {
                tape_uuid: supplied_uuid.expect("validated supplied UUID"),
                block_size: hints.block_size_bytes,
                scheme: remanence_parity::parse_parity_arg(&scheme_arg)
                    .map_err(|error| error.to_string())?,
            })
        })
        .transpose()?;
    let first_bootstrap = match discover_bootstrap_with_recovery_hints(
        source,
        &candidates,
        scan_hints.as_ref(),
    ) {
        Ok(bootstrap) => Some(bootstrap),
        Err(ParityError::NoBootstrapFound | ParityError::BootstrapParse(_)) if hints.is_some() => {
            None
        }
        Err(error @ (ParityError::NoBootstrapFound | ParityError::BootstrapParse(_))) => {
            return Err(format!("discover bootstrap: {error}; unreadable bootstrap requires --tape-uuid, --block-size and --scheme"));
        }
        Err(error) => return Err(format!("discover bootstrap: {error}")),
    };
    let (tape_uuid, block_size) = match (&first_bootstrap, &hints) {
        (Some(bootstrap), _) => (bootstrap.tape_uuid, bootstrap.block_size_bytes),
        (None, Some(hints)) => (
            supplied_uuid.expect("validated supplied UUID"),
            hints.block_size_bytes,
        ),
        (None, None) => unreachable!("bootstrap discovery requires hints to fall back"),
    };
    let mode = scan_hints
        .as_ref()
        .map_or(ScanMode::Standard, ScanMode::Recovery);
    let scan = scan_reconstruct_filemark_map_with_report_mode(source, &tape_uuid, block_size, mode)
        .map_err(|error| format!("scan filemark map: {error}"))?;
    let bootstrap_treated_as_unreadable = scan.bootstrap_recovery_hints.is_some();
    // A retry in the structural walk can recover a bootstrap that discovery
    // could not read. Preserve that validated evidence in the report as well.
    let first_bootstrap = first_bootstrap.or_else(|| {
        scan.authoritative_bootstrap()
            .map(|candidate| candidate.payload.clone())
    });
    let mut terminal_entries = Vec::new();
    let mut terminal_rows = Vec::new();
    let inventory = read_terminal_index_inventory(
        source,
        &tape_uuid,
        block_size,
        |entry| {
            terminal_entries.push(entry.clone());
            Ok(())
        },
        |row| {
            terminal_rows.push(row.clone());
            Ok(())
        },
    )
    .map_err(|error| format!("read terminal tape index: {error}"))?;
    let recovered = match inventory {
        TerminalInventoryOutcome::Inventory(selection) => {
            let map = FilemarkMap::new(
                terminal_entries
                    .into_iter()
                    .map(terminal_entry_to_filemark_entry)
                    .collect(),
            )
            .map_err(|error| format!("build terminal-index filemark map: {error}"))?;
            RecoveredMap {
                scope_tape_file_count: map.tape_file_count(),
                map,
                object_rows: terminal_rows,
                bootstrap_generation_used: first_bootstrap
                    .as_ref()
                    .map(|bootstrap| bootstrap.sequence),
                overlay_source: selected_terminal_replica_name(selection.selected_replica_ordinal),
                terminal_authority: true,
                damaged_regions: scan.damaged_regions,
                truncation: scan.truncation,
            }
        }
        TerminalInventoryOutcome::BotStructuralRecoveryRequired(_) => RecoveredMap {
            scope_tape_file_count: scan.map.tape_file_count(),
            map: scan.map,
            object_rows: Vec::new(),
            bootstrap_generation_used: first_bootstrap.as_ref().map(|bootstrap| bootstrap.sequence),
            overlay_source: "structural_walk_no_terminal_authority",
            terminal_authority: false,
            damaged_regions: scan.damaged_regions,
            truncation: scan.truncation,
        },
    };

    let mut totals = RecoveryTotals {
        objects_seen: if recovered.terminal_authority {
            u64::try_from(recovered.object_rows.len())
                .map_err(|_| "terminal Object-row count exceeds u64::MAX".to_string())?
        } else {
            recovered
                .map
                .entries()
                .iter()
                .filter(|entry| entry.kind == TapeFileKind::Object)
                .count()
                .try_into()
                .map_err(|_| "structural Object count exceeds u64::MAX".to_string())?
        },
        ..RecoveryTotals::default()
    };
    let mut objects = Vec::with_capacity(recovered.object_rows.len());
    for row in &recovered.object_rows {
        let object = report_object_row(
            source,
            &recovered.map,
            recovered.scope_tape_file_count,
            block_size,
            row,
        );
        match object.map_status {
            "map_agrees" => totals.map_agreeing += 1,
            "beyond_recovered_scope" => totals.beyond_scope += 1,
            _ => {}
        }
        if object.map_status == "map_agrees"
            && matches!(
                object.verification_status,
                "manifest_verified" | "envelope_consistent"
            )
        {
            totals.verified += 1;
        } else if object.map_status != "beyond_recovered_scope" {
            totals.failed += 1;
        }
        objects.push(object);
    }
    if !recovered.terminal_authority {
        for entry in recovered
            .map
            .entries()
            .iter()
            .filter(|entry| entry.kind == TapeFileKind::Object)
        {
            totals.map_agreeing = totals
                .map_agreeing
                .checked_add(1)
                .ok_or_else(|| "recovery map-agreement count overflows u64".to_string())?;
            totals.failed = totals
                .failed
                .checked_add(1)
                .ok_or_else(|| "recovery failure count overflows u64".to_string())?;
            objects.push(unknown_structural_object(entry));
        }
    }

    Ok(CatalogLessRecoveryReport {
        report_version: 1,
        tape_uuid: first_bootstrap
            .as_ref()
            .map(|bootstrap| hex(&bootstrap.tape_uuid)),
        block_size_bytes: first_bootstrap
            .as_ref()
            .map(|bootstrap| bootstrap.block_size_bytes),
        supplied: hints,
        scan: RecoveryScanSummary {
            bootstrap_treated_as_unreadable,
            identity_and_geometry_from_hints: first_bootstrap.is_none(),
            bootstrap_generation_used: recovered.bootstrap_generation_used,
            bootstrap_tape_file_number: first_bootstrap.as_ref().map(|_| 0),
            overlay_source: recovered.overlay_source,
            recovered_scope_tape_file_count: recovered.scope_tape_file_count,
            damaged_regions: recovered
                .damaged_regions
                .into_iter()
                .map(RecoveryDamageRegion::from)
                .collect(),
            truncation: recovered.truncation.map(RecoveryTruncation::from),
        },
        objects,
        totals,
        success: totals.failed == 0,
    })
}

fn terminal_entry_to_filemark_entry(entry: TapeIndexReplicaMapEntry) -> TapeFileMapEntry {
    TapeFileMapEntry {
        tape_file_number: entry.tape_file_number,
        kind: match entry.kind {
            TapeIndexReplicaFileKind::Object => TapeFileKind::Object,
            TapeIndexReplicaFileKind::ParitySidecar => TapeFileKind::ParitySidecar,
            TapeIndexReplicaFileKind::Bootstrap => TapeFileKind::Bootstrap,
            TapeIndexReplicaFileKind::ParityMap => TapeFileKind::ParityMap,
            TapeIndexReplicaFileKind::TapeIndexReplica => TapeFileKind::TapeIndexReplica,
            TapeIndexReplicaFileKind::IndexSeparationExtent => TapeFileKind::IndexSeparationExtent,
        },
        block_count: entry.block_count,
        first_parity_data_ordinal: entry.first_parity_data_ordinal,
        protected_ordinal_start: entry.protected_ordinal_start,
        protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
        epoch_id: entry.epoch_id,
    }
}

fn selected_terminal_replica_name(ordinal: u16) -> &'static str {
    match ordinal {
        1 => "terminal_index_replica_a",
        2 => "terminal_index_replica_b",
        3 => "terminal_index_replica_c",
        _ => "terminal_index_replica_unknown",
    }
}

fn unknown_structural_object(entry: &TapeFileMapEntry) -> RecoveryObjectReport {
    RecoveryObjectReport {
        tape_file_number: entry.tape_file_number,
        representation: "unknown",
        object_id: Value::Null,
        object_id_encoding: "unavailable",
        object_id_human: "<identity unavailable>".to_string(),
        stored_block_count: entry.block_count,
        map_status: "map_agrees",
        map_detail: None,
        verification_status: "terminal_authority_unavailable",
        verification_detail: Some(
            "no valid terminal tape-index replica survived; structural recovery cannot prove Object identity"
                .to_string(),
        ),
    }
}

fn report_object_row(
    source: &mut dyn RawTapeSource,
    map: &FilemarkMap,
    recovered_scope_tape_file_count: u64,
    block_size: u32,
    row: &TapeIndexReplicaObjectRow,
) -> RecoveryObjectReport {
    let (object_id, object_id_encoding, object_id_human) =
        render_object_id(Some(row.object_id.as_slice()));
    let representation = match row.representation {
        ObjectRecoveryRepresentation::Plaintext { .. } => "plaintext",
        ObjectRecoveryRepresentation::Encrypted { .. } => "encrypted",
    };
    let mut report = RecoveryObjectReport {
        tape_file_number: row.tape_file_number,
        representation,
        object_id,
        object_id_encoding,
        object_id_human,
        stored_block_count: row.stored_block_count,
        map_status: "map_mismatch",
        map_detail: None,
        verification_status: "not_checked",
        verification_detail: None,
    };

    if row.tape_file_number >= recovered_scope_tape_file_count {
        report.map_status = "beyond_recovered_scope";
        report.map_detail = Some(format!(
            "tape file {} lies outside recovered/attested prefix 0..{}",
            row.tape_file_number, recovered_scope_tape_file_count
        ));
        report.verification_status = "not_checked_beyond_scope";
        return report;
    }

    let map_index = match usize::try_from(row.tape_file_number) {
        Ok(index) => index,
        Err(_) => {
            report.map_detail = Some(format!(
                "tape file {} cannot be indexed in this reader's address space",
                row.tape_file_number
            ));
            return report;
        }
    };
    let Some(entry) = map.entries().get(map_index) else {
        report.map_detail = Some(format!(
            "tape file {} is absent from the recovered map",
            row.tape_file_number
        ));
        return report;
    };
    if entry.kind != TapeFileKind::Object {
        report.map_detail = Some(format!(
            "recovered map classifies tape file {} as {:?}, expected Object",
            row.tape_file_number, entry.kind
        ));
        return report;
    }
    if entry.block_count != row.stored_block_count {
        report.map_detail = Some(format!(
            "stored block count mismatch: row {}, recovered map {}",
            row.stored_block_count, entry.block_count
        ));
        return report;
    }
    report.map_status = "map_agrees";

    let verification = match &row.representation {
        ObjectRecoveryRepresentation::Plaintext {
            manifest_first_chunk_lba,
            manifest_size_bytes,
            manifest_chunk_count,
            manifest_sha256,
        } => verify_plaintext_manifest(
            source,
            map,
            row,
            block_size,
            *manifest_first_chunk_lba,
            *manifest_size_bytes,
            *manifest_chunk_count,
            manifest_sha256,
        ),
        ObjectRecoveryRepresentation::Encrypted {
            recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        } => verify_encrypted_envelope(
            source,
            map,
            row,
            block_size,
            recipient_epoch_ids,
            *metadata_frame_len,
            *key_frame_len,
        ),
    };
    report.verification_status = verification.status;
    report.verification_detail = verification.detail;
    report
}

struct Verification {
    status: &'static str,
    detail: Option<String>,
}

impl Verification {
    fn success(status: &'static str) -> Self {
        Self {
            status,
            detail: None,
        }
    }

    fn failure(status: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: Some(detail.into()),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn verify_plaintext_manifest(
    source: &mut dyn RawTapeSource,
    map: &FilemarkMap,
    row: &TapeIndexReplicaObjectRow,
    block_size: u32,
    manifest_first_chunk_lba: u64,
    manifest_size_bytes: u64,
    manifest_chunk_count: u64,
    expected_digest: &[u8; 32],
) -> Verification {
    let Some(manifest_end) = manifest_first_chunk_lba.checked_add(manifest_chunk_count) else {
        return Verification::failure(
            "manifest_bounds_violation",
            "manifest block range overflows u64",
        );
    };
    let Some(manifest_capacity) = manifest_chunk_count.checked_mul(u64::from(block_size)) else {
        return Verification::failure(
            "manifest_bounds_violation",
            "manifest byte capacity overflows u64",
        );
    };
    if manifest_chunk_count == 0
        || manifest_size_bytes == 0
        || manifest_end > row.stored_block_count
        || manifest_size_bytes > manifest_capacity
    {
        return Verification::failure(
            "manifest_bounds_violation",
            format!(
                "manifest range [{manifest_first_chunk_lba}, {manifest_end}) size {manifest_size_bytes} exceeds row extent {} blocks at {block_size} bytes",
                row.stored_block_count
            ),
        );
    }

    let mut hasher = Sha256::new();
    let mut remaining = manifest_size_bytes;
    let mut unreadable = Vec::new();
    let mut block = vec![0u8; block_size as usize];
    for offset in 0..manifest_chunk_count {
        let block_within_file = manifest_first_chunk_lba + offset;
        match read_object_block(
            source,
            map,
            row.tape_file_number,
            block_within_file,
            &mut block,
        ) {
            Ok(()) => {
                let take = match usize::try_from(remaining.min(u64::from(block_size))) {
                    Ok(take) => take,
                    Err(_) => {
                        return Verification::failure(
                            "manifest_bounds_violation",
                            "manifest block slice length does not fit usize",
                        );
                    }
                };
                hasher.update(&block[..take]);
                remaining -= take as u64;
            }
            Err(detail) => unreadable.push(format!("block {block_within_file}: {detail}")),
        }
    }
    if !unreadable.is_empty() {
        return Verification::failure("manifest_unreadable_blocks", unreadable.join("; "));
    }
    if remaining != 0 {
        return Verification::failure(
            "manifest_bounds_violation",
            format!("{remaining} manifest bytes remained after declared chunks"),
        );
    }
    let actual: [u8; 32] = hasher.finalize().into();
    if actual != *expected_digest {
        return Verification::failure(
            "manifest_digest_mismatch",
            format!(
                "expected {}, measured {}",
                hex(expected_digest),
                hex(&actual)
            ),
        );
    }
    Verification::success("manifest_verified")
}

fn verify_encrypted_envelope(
    source: &mut dyn RawTapeSource,
    map: &FilemarkMap,
    row: &TapeIndexReplicaObjectRow,
    block_size: u32,
    expected_recipient_epoch_ids: &[[u8; 16]],
    expected_metadata_frame_len: u64,
    expected_key_frame_len: u32,
) -> Verification {
    let mut first_block = vec![0u8; block_size as usize];
    if let Err(detail) = read_object_block(source, map, row.tape_file_number, 0, &mut first_block) {
        return Verification::failure("envelope_header_unreadable", detail);
    }
    let header_bytes: [u8; REM_OBJECT_HEADER_LEN] = match first_block
        .get(..REM_OBJECT_HEADER_LEN)
        .and_then(|bytes| bytes.try_into().ok())
    {
        Some(bytes) => bytes,
        None => {
            return Verification::failure(
                "envelope_header_invalid",
                format!("tape block size {block_size} is shorter than the envelope header"),
            );
        }
    };
    let header = match RemObjectHeader::parse(&header_bytes) {
        Ok(header) => header,
        Err(error) => {
            return Verification::failure("envelope_header_invalid", error.to_string());
        }
    };
    if header.chunk_size != block_size {
        return Verification::failure(
            "envelope_length_inconsistent",
            format!(
                "envelope chunk size {} differs from tape block size {block_size}",
                header.chunk_size
            ),
        );
    }
    if header.metadata_frame_len != expected_metadata_frame_len {
        return Verification::failure(
            "envelope_metadata_frame_len_mismatch",
            format!(
                "row {}, envelope header {}",
                expected_metadata_frame_len, header.metadata_frame_len
            ),
        );
    }
    if header.key_frame_len != expected_key_frame_len {
        return Verification::failure(
            "envelope_key_frame_len_mismatch",
            format!(
                "row {}, envelope header {}",
                expected_key_frame_len, header.key_frame_len
            ),
        );
    }

    let prefix_len = match REM_OBJECT_HEADER_LEN.checked_add(header.key_frame_len as usize) {
        Some(length) => length,
        None => {
            return Verification::failure(
                "envelope_length_inconsistent",
                "header plus key-frame length overflows usize",
            );
        }
    };
    let prefix = match read_object_prefix(
        source,
        map,
        row.tape_file_number,
        row.stored_block_count,
        block_size,
        prefix_len,
    ) {
        Ok(prefix) => prefix,
        Err(detail) => {
            return Verification::failure("envelope_key_frame_unreadable", detail);
        }
    };
    let key_frame = match KeyFrame::parse(&prefix[REM_OBJECT_HEADER_LEN..prefix_len]) {
        Ok(key_frame) => key_frame,
        Err(error) => {
            return Verification::failure("envelope_key_frame_invalid", error.to_string());
        }
    };
    let measured_epoch_ids: Vec<_> = key_frame
        .slots
        .iter()
        .map(|slot| slot.recipient_epoch_id)
        .collect();
    if measured_epoch_ids != expected_recipient_epoch_ids {
        return Verification::failure(
            "envelope_recipient_epoch_ids_mismatch",
            format!(
                "row [{}], key frame [{}]",
                expected_recipient_epoch_ids
                    .iter()
                    .map(|id| hex(id))
                    .collect::<Vec<_>>()
                    .join(","),
                measured_epoch_ids
                    .iter()
                    .map(|id| hex(id))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        );
    }

    let stored_size = match row.stored_block_count.checked_mul(u64::from(block_size)) {
        Some(size) => size,
        None => {
            return Verification::failure(
                "envelope_length_inconsistent",
                "measured tape-file byte length overflows u64",
            );
        }
    };
    let footer_offset = match validate_envelope_geometry(&header, stored_size) {
        Ok(footer_offset) => footer_offset,
        Err(detail) => {
            return Verification::failure("envelope_length_inconsistent", detail);
        }
    };
    if let Err(failure) = verify_envelope_completion(
        source,
        map,
        row.tape_file_number,
        block_size,
        footer_offset,
        stored_size,
    ) {
        return failure;
    }
    Verification::success("envelope_consistent")
}

fn validate_envelope_geometry(header: &RemObjectHeader, stored_size: u64) -> Result<u64, String> {
    let key_frame_len = u64::from(header.key_frame_len);
    let fixed_without_chunks = (REM_OBJECT_HEADER_LEN as u64)
        .checked_add(key_frame_len)
        .and_then(|value| value.checked_add(header.metadata_frame_len))
        .and_then(|value| value.checked_add(REM_OBJECT_FOOTER.len() as u64))
        .ok_or_else(|| "envelope fixed-frame lengths overflow u64".to_string())?;
    let available_for_chunks = stored_size.checked_sub(fixed_without_chunks).ok_or_else(|| {
        format!(
            "measured tape-file length {stored_size} is shorter than fixed envelope frames {fixed_without_chunks}"
        )
    })?;
    let stride = u64::from(header.chunk_size)
        .checked_add(16)
        .ok_or_else(|| "envelope chunk stride overflows u64".to_string())?;
    let chunk_count = available_for_chunks / stride;
    if chunk_count == 0 {
        return Err("measured tape-file length contains no payload chunk".to_string());
    }
    let footer_end = (REM_OBJECT_HEADER_LEN as u64)
        .checked_add(key_frame_len)
        .and_then(|value| value.checked_add(header.metadata_frame_len))
        .and_then(|value| value.checked_add(chunk_count.checked_mul(stride)?))
        .and_then(|value| value.checked_add(REM_OBJECT_FOOTER.len() as u64))
        .ok_or_else(|| "envelope measured-length arithmetic overflows u64".to_string())?;
    let expected_stored_size = round_up(footer_end, u64::from(header.chunk_size))?;
    if expected_stored_size != stored_size {
        return Err(format!(
            "measured tape-file length {stored_size} is inconsistent with {} chunks (expected {expected_stored_size})",
            chunk_count
        ));
    }
    Ok(footer_end - REM_OBJECT_FOOTER.len() as u64)
}

fn verify_envelope_completion(
    source: &mut dyn RawTapeSource,
    map: &FilemarkMap,
    tape_file_number: u64,
    block_size: u32,
    footer_offset: u64,
    stored_size: u64,
) -> Result<(), Verification> {
    if block_size == 0 {
        return Err(Verification::failure(
            "envelope_length_inconsistent",
            "envelope block size is zero",
        ));
    }
    let footer_end = footer_offset
        .checked_add(REM_OBJECT_FOOTER.len() as u64)
        .ok_or_else(|| {
            Verification::failure(
                "envelope_length_inconsistent",
                "envelope footer end overflows u64",
            )
        })?;
    if footer_end > stored_size {
        return Err(Verification::failure(
            "envelope_length_inconsistent",
            "envelope footer lies beyond measured tape-file length",
        ));
    }

    let block_size_u64 = u64::from(block_size);
    let first_block = footer_offset / block_size_u64;
    let final_block = stored_size.checked_sub(1).ok_or_else(|| {
        Verification::failure(
            "envelope_length_inconsistent",
            "envelope stored length is zero",
        )
    })? / block_size_u64;
    let mut block = vec![0u8; block_size as usize];
    for block_within_file in first_block..=final_block {
        if let Err(detail) =
            read_object_block(source, map, tape_file_number, block_within_file, &mut block)
        {
            return Err(Verification::failure(
                "envelope_completion_unreadable",
                format!("block {block_within_file}: {detail}"),
            ));
        }
        let block_start = block_within_file
            .checked_mul(block_size_u64)
            .ok_or_else(|| {
                Verification::failure(
                    "envelope_length_inconsistent",
                    "envelope completion block offset overflows u64",
                )
            })?;
        for (offset, byte) in block.iter().copied().enumerate() {
            let absolute = block_start.checked_add(offset as u64).ok_or_else(|| {
                Verification::failure(
                    "envelope_length_inconsistent",
                    "envelope completion byte offset overflows u64",
                )
            })?;
            if absolute < footer_offset || absolute >= stored_size {
                continue;
            }
            let expected = if absolute < footer_end {
                REM_OBJECT_FOOTER[(absolute - footer_offset) as usize]
            } else {
                0
            };
            if byte != expected {
                let region = if absolute < footer_end {
                    "footer"
                } else {
                    "zero fill"
                };
                return Err(Verification::failure(
                    "envelope_completion_invalid",
                    format!(
                        "{region} mismatch at byte offset {absolute}: expected {expected:#04x}, measured {byte:#04x}"
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn round_up(value: u64, alignment: u64) -> Result<u64, String> {
    if alignment == 0 {
        return Err("envelope chunk alignment is zero".to_string());
    }
    let remainder = value % alignment;
    if remainder == 0 {
        Ok(value)
    } else {
        value
            .checked_add(alignment - remainder)
            .ok_or_else(|| "envelope padded length overflows u64".to_string())
    }
}

fn read_object_prefix(
    source: &mut dyn RawTapeSource,
    map: &FilemarkMap,
    tape_file_number: u64,
    stored_block_count: u64,
    block_size: u32,
    prefix_len: usize,
) -> Result<Vec<u8>, String> {
    let block_size_usize = block_size as usize;
    if block_size_usize == 0 {
        return Err("envelope block size is zero".to_string());
    }
    let block_count = prefix_len
        .checked_add(block_size_usize - 1)
        .ok_or_else(|| "envelope prefix block count overflows usize".to_string())?
        / block_size_usize;
    if u64::try_from(block_count).map_err(|_| "prefix block count exceeds u64".to_string())?
        > stored_block_count
    {
        return Err(format!(
            "envelope prefix requires {block_count} blocks, tape file has {stored_block_count}"
        ));
    }
    let capacity = block_count
        .checked_mul(block_size_usize)
        .ok_or_else(|| "envelope prefix allocation overflows usize".to_string())?;
    let mut prefix = Vec::with_capacity(capacity);
    let mut block = vec![0u8; block_size_usize];
    let mut unreadable = Vec::new();
    for block_within_file in 0..block_count {
        match read_object_block(
            source,
            map,
            tape_file_number,
            block_within_file as u64,
            &mut block,
        ) {
            Ok(()) => prefix.extend_from_slice(&block),
            Err(detail) => unreadable.push(format!("block {block_within_file}: {detail}")),
        }
    }
    if !unreadable.is_empty() {
        return Err(unreadable.join("; "));
    }
    prefix.truncate(prefix_len);
    Ok(prefix)
}

fn read_object_block(
    source: &mut dyn RawTapeSource,
    map: &FilemarkMap,
    tape_file_number: u64,
    block_within_file: u64,
    buf: &mut [u8],
) -> Result<(), String> {
    let position = map
        .physical_position(TapeFilePosition {
            tape_file_number,
            block_within_file,
        })
        .map_err(|error| format!("bounds/position error: {error}"))?;
    source
        .locate_physical(position)
        .map_err(|error| format!("locate LBA {}: {error}", position.lba))?;
    match source.read_record(buf) {
        Ok(RawReadOutcome::Block { bytes, .. }) if bytes == buf.len() => Ok(()),
        Ok(RawReadOutcome::Block { bytes, .. }) => Err(format!(
            "short block at LBA {}: {bytes} bytes",
            position.lba
        )),
        Ok(RawReadOutcome::Filemark { .. }) => {
            Err(format!("unexpected filemark at LBA {}", position.lba))
        }
        Ok(RawReadOutcome::EndOfData { .. }) => {
            Err(format!("unexpected end of data at LBA {}", position.lba))
        }
        Err(error) => Err(format!("unreadable at LBA {}: {error}", position.lba)),
    }
}

fn render_object_id(object_id: Option<&[u8]>) -> (Value, &'static str, String) {
    let Some(bytes) = object_id else {
        let absent = "absent(minor<=2)".to_string();
        return (Value::String(absent.clone()), "absent", absent);
    };
    let human = String::from_utf8_lossy(bytes).into_owned();
    match std::str::from_utf8(bytes) {
        Ok(value) => (Value::String(value.to_string()), "utf8", human),
        Err(_) => (
            Value::String(BASE64_STANDARD.encode(bytes)),
            "base64",
            human,
        ),
    }
}

fn print_human_report(
    report: &CatalogLessRecoveryReport,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "catalog-less recovery report")?;
    writeln!(
        out,
        "tape uuid: {}",
        report.tape_uuid.as_deref().unwrap_or("unknown")
    )?;
    writeln!(
        out,
        "block size: {}",
        report
            .block_size_bytes
            .map_or_else(|| "unknown".to_string(), |size| format!("{size} bytes"))
    )?;
    if let Some(hints) = &report.supplied {
        writeln!(
            out,
            "supplied: tape uuid {} block size {} bytes scheme {}",
            hints.tape_uuid, hints.block_size_bytes, hints.scheme
        )?;
    }
    if report.scan.bootstrap_treated_as_unreadable {
        writeln!(
            out,
            "bootstrap treated as unreadable during scan; identity and geometry supplied by {}",
            if report.scan.identity_and_geometry_from_hints {
                "hints"
            } else {
                "validated bootstrap"
            }
        )?;
    }
    writeln!(
        out,
        "scan: bootstrap generation {} tape-file {} overlay {} damaged-regions {} scope-files {}",
        report.scan.bootstrap_generation_used.map_or_else(
            || "unknown".to_string(),
            |generation| generation.to_string()
        ),
        report
            .scan
            .bootstrap_tape_file_number
            .map_or_else(|| "unknown".to_string(), |number| number.to_string()),
        report.scan.overlay_source,
        report.scan.damaged_regions.len(),
        report.scan.recovered_scope_tape_file_count
    )?;
    if let Some(truncation) = &report.scan.truncation {
        writeln!(
            out,
            "scan tail: tape-file {} LBA {} {}",
            truncation.tape_file_number, truncation.start_lba, truncation.kind
        )?;
    }
    for damage in &report.scan.damaged_regions {
        writeln!(
            out,
            "scan damage: partition {} LBA {} blocks={} {}",
            damage.partition, damage.start_lba, damage.block_count, damage.kind
        )?;
    }
    for object in &report.objects {
        writeln!(
            out,
            "object tape-file {} {} object_id={} blocks={}",
            object.tape_file_number,
            object.representation,
            object.object_id_human,
            object.stored_block_count
        )?;
        writeln!(
            out,
            "  map: {}{}",
            object.map_status,
            object
                .map_detail
                .as_ref()
                .map_or_else(String::new, |detail| format!(" ({detail})"))
        )?;
        writeln!(
            out,
            "  verify: {}{}",
            object.verification_status,
            object
                .verification_detail
                .as_ref()
                .map_or_else(String::new, |detail| format!(" ({detail})"))
        )?;
    }
    writeln!(
        out,
        "totals: objects={} map-agreeing={} verified={} failed={} beyond-scope={}",
        report.totals.objects_seen,
        report.totals.map_agreeing,
        report.totals.verified,
        report.totals.failed,
        report.totals.beyond_scope
    )?;
    writeln!(
        out,
        "result: {}",
        if report.success { "verified" } else { "failed" }
    )
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;

    use clap::Parser as _;
    use remanence_aead::{
        seal_deterministic_for_test_vectors, DataEncryptionKey, EnvelopeSealOptions,
        RecipientPrivateKey, SealOptions, SealReport,
    };
    use remanence_parity::bootstrap::write_bootstrap_block;
    use remanence_parity::{
        checked_tape_index_replica_layout, default_scheme_for_block_size, plan_index_separation,
        plan_tape_index_edition, plan_tape_index_replica, write_index_separation,
        write_tape_index_replica, BootstrapPayload, IndexSeparationDescriptor,
        IndexSeparationObservation, ParityError, ParitySchemeRecord, RawTapeSource,
        TapeIndexEditionDescriptor, TapeIndexReplicaCounts, TapeIndexReplicaObservation,
        TapeIndexReplicaRecordSource, TapeIndexReplicaScope, TerminalTailLayout,
    };
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    const BLOCK_SIZE: u32 = 262_144;
    const TAPE_UUID: [u8; 16] = [0x42; 16];
    const OBJECT_ID: &str = "00000000-0000-4000-8000-000000000001";

    #[test]
    fn debug_cli_parses_documented_recovery_report_syntax() {
        let cli = crate::DebugCli::try_parse_from([
            "rem-debug",
            "tape",
            "recovery-report",
            "/tmp/published-image",
            "--json",
        ])
        .expect("documented recovery-report syntax parses");
        match cli.command {
            crate::Command::Tape {
                command: crate::TapeCommand::RecoveryReport(args),
            } => {
                assert_eq!(args.source, Path::new("/tmp/published-image"));
                assert!(args.json);
            }
            other => panic!("unexpected parsed command: {other:?}"),
        }
    }

    /// Exercise the public CLI over freshly generated images, including provenance.
    #[test]
    fn recovery_report_unreadable_bootstrap_requires_all_hints() {
        for parity in [false, true] {
            for absent in [false, true] {
                let mut files = plaintext_image(parity);
                if absent {
                    files[0].fill(0);
                } else {
                    // Keep the magic but invalidate the bootstrap checksum.
                    files[0][80] ^= 1;
                }
                let temp = write_image_directory(&files);
                let mut out = Vec::new();
                let mut err = Vec::new();
                assert_eq!(
                    run_image_recovery_report(temp.path(), None, true, &mut out, &mut err),
                    ExitCode::from(1)
                );
                assert!(String::from_utf8_lossy(&err).contains("requires --tape-uuid"));
                assert!(out.is_empty());

                let scheme = if parity {
                    let scheme = parity_scheme_record();
                    format!(
                        "{},{},{}",
                        scheme.data_blocks_per_stripe,
                        scheme.parity_blocks_per_stripe,
                        scheme.stripes_per_neighborhood
                    )
                } else {
                    "none".to_string()
                };
                let uuid = uuid::Uuid::from_bytes(TAPE_UUID).to_string();
                let cli = crate::DebugCli::try_parse_from([
                    "rem-debug",
                    "tape",
                    "recovery-report",
                    temp.path().to_str().expect("image path"),
                    "--json",
                    "--tape-uuid",
                    &uuid,
                    "--block-size",
                    "262144",
                    "--scheme",
                    &scheme,
                ])
                .expect("complete hints parse");
                let crate::Command::Tape {
                    command: crate::TapeCommand::RecoveryReport(args),
                } = cli.command
                else {
                    panic!("expected recovery report command");
                };
                err.clear();
                assert_eq!(
                    run_image_recovery_report(
                        &args.source,
                        args.hints(),
                        args.json,
                        &mut out,
                        &mut err
                    ),
                    ExitCode::SUCCESS,
                    "{}",
                    String::from_utf8_lossy(&err)
                );
                let report: Value = serde_json::from_slice(&out).expect("JSON report");
                assert!(report["tape_uuid"].is_null());
                assert!(report["block_size_bytes"].is_null());
                assert!(report["scan"]["bootstrap_generation_used"].is_null());
                assert_eq!(report["scan"]["bootstrap_treated_as_unreadable"], true);
                assert_eq!(report["scan"]["identity_and_geometry_from_hints"], true);
                assert!(report["scan"]["bootstrap_tape_file_number"].is_null());
                assert_eq!(
                    report["supplied"],
                    json!({"tape_uuid": uuid, "block_size_bytes": BLOCK_SIZE, "scheme": scheme})
                );
                assert_eq!(report["totals"]["verified"], "1");
                assert_eq!(report["scan"]["overlay_source"], "terminal_index_replica_c");
            }
        }
    }

    /// Physical BOT read damage also uses supplied authority without inventing bootstrap evidence.
    #[test]
    fn recovery_report_medium_error_at_bootstrap_uses_hints() {
        let temp = write_image_directory(&plaintext_image(false));
        let mut source = ImageDirectoryRawSource::open(temp.path()).expect("open image");
        source.mark_unreadable(0, 0).expect("BOT block exists");
        let candidates = source.candidate_block_sizes().to_vec();
        assert!(build_recovery_report(&mut source, &candidates, None).is_err());
        let report = build_recovery_report(
            &mut source,
            &candidates,
            Some(RecoveryHints {
                tape_uuid: uuid::Uuid::from_bytes(TAPE_UUID).to_string(),
                block_size_bytes: BLOCK_SIZE,
                scheme: "none".to_string(),
            }),
        )
        .expect("terminal discovery survives BOT medium error");
        assert!(report.success);
        assert_eq!(report.totals.verified, 1);
        assert!(report.tape_uuid.is_none());
        assert!(report.scan.bootstrap_generation_used.is_none());
        assert_eq!(report.scan.damaged_regions.len(), 1);
        let mut human = Vec::new();
        print_human_report(&report, &mut human).expect("human report");
        let human = String::from_utf8(human).expect("UTF-8 report");
        assert!(human.contains("tape uuid: unknown"));
        assert!(human.lines().any(|line| line == "block size: unknown"));
        assert!(human.contains("supplied: tape uuid 42424242-4242-4242-4242-424242424242 block size 262144 bytes scheme none"));
    }

    /// A valid bootstrap remains authoritative and refuses every hint conflict.
    #[test]
    fn recovery_report_readable_bootstrap_rejects_conflicting_hints() {
        let temp = write_image_directory(&plaintext_image(false));
        let mut hints = RecoveryHints {
            tape_uuid: uuid::Uuid::from_bytes([0x99; 16]).to_string(),
            block_size_bytes: 524288,
            scheme: "128,4,1".to_string(),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            run_image_recovery_report(temp.path(), Some(hints.clone()), true, &mut out, &mut err),
            ExitCode::from(1)
        );
        assert!(String::from_utf8_lossy(&err).contains("tape identity mismatch"));
        assert!(out.is_empty());
        hints.tape_uuid = uuid::Uuid::from_bytes(TAPE_UUID).to_string();
        for mismatch in ["block size", "parity scheme"] {
            err.clear();
            assert_eq!(
                run_image_recovery_report(
                    temp.path(),
                    Some(hints.clone()),
                    true,
                    &mut out,
                    &mut err
                ),
                ExitCode::from(1)
            );
            assert!(
                String::from_utf8_lossy(&err).contains(mismatch),
                "{}",
                String::from_utf8_lossy(&err)
            );
            assert!(out.is_empty());
            hints.block_size_bytes = BLOCK_SIZE;
        }
        hints.scheme = "none".to_string();
        err.clear();
        assert_eq!(
            run_image_recovery_report(temp.path(), Some(hints), true, &mut out, &mut err),
            ExitCode::SUCCESS,
            "{}",
            String::from_utf8_lossy(&err)
        );
        let report: Value = serde_json::from_slice(&out).expect("JSON report");
        assert_eq!(report["block_size_bytes"], BLOCK_SIZE);
        assert_eq!(report["scan"]["bootstrap_generation_used"], "0");
        assert_eq!(report["scan"]["bootstrap_treated_as_unreadable"], false);
        assert_eq!(report["scan"]["identity_and_geometry_from_hints"], false);
    }

    /// A readable bootstrap found on the walk takes precedence after failed probes.
    #[test]
    fn recovery_report_keeps_bootstrap_evidence_recovered_during_scan() {
        struct FailedProbes {
            inner: ImageDirectoryRawSource,
            probes_left: usize,
            fail_after_discovery: bool,
        }
        impl RawTapeSource for FailedProbes {
            fn configure_fixed_block_size(&mut self, size: u32) -> Result<(), ParityError> {
                self.inner.configure_fixed_block_size(size)
            }
            fn locate_physical(
                &mut self,
                hint: remanence_parity::PhysicalPositionHint,
            ) -> Result<(), ParityError> {
                self.inner.locate_physical(hint)
            }
            fn locate_end_of_data(
                &mut self,
            ) -> Result<remanence_parity::PhysicalPositionHint, ParityError> {
                self.inner.locate_end_of_data()
            }
            fn space_filemarks(
                &mut self,
                count: i64,
            ) -> Result<remanence_parity::SpaceFilemarksOutcome, ParityError> {
                self.inner.space_filemarks(count)
            }
            fn position(&mut self) -> Result<remanence_parity::PhysicalPositionHint, ParityError> {
                self.inner.position()
            }
            fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
                if self.fail_after_discovery && self.probes_left == 0 {
                    self.inner.mark_unreadable(0, 0).expect("BOT exists");
                }
                let outcome = self.inner.read_record(buf)?;
                if self.probes_left > 0 {
                    self.probes_left -= 1;
                    if !self.fail_after_discovery {
                        buf.fill(0);
                    }
                }
                Ok(outcome)
            }
        }
        for fail_after_discovery in [false, true] {
            let temp = write_image_directory(&plaintext_image(false));
            let mut source = FailedProbes {
                inner: ImageDirectoryRawSource::open(temp.path()).expect("image"),
                probes_left: if fail_after_discovery { 1 } else { 2 },
                fail_after_discovery,
            };
            let report = build_recovery_report(
                &mut source,
                &[BLOCK_SIZE],
                Some(RecoveryHints {
                    tape_uuid: uuid::Uuid::from_bytes(TAPE_UUID).to_string(),
                    block_size_bytes: BLOCK_SIZE,
                    scheme: "none".to_string(),
                }),
            )
            .expect("the structural scan reads a valid bootstrap");
            assert!(report.success);
            assert_eq!(source.probes_left, 0);
            assert_eq!(report.tape_uuid.as_deref(), Some(hex(&TAPE_UUID).as_str()));
            assert_eq!(report.block_size_bytes, Some(BLOCK_SIZE));
            assert_eq!(report.scan.bootstrap_generation_used, Some(0));
            assert_eq!(
                report.scan.bootstrap_treated_as_unreadable,
                fail_after_discovery
            );
            assert!(!report.scan.identity_and_geometry_from_hints);
            let mut human = Vec::new();
            print_human_report(&report, &mut human).expect("human report");
            if fail_after_discovery {
                assert!(String::from_utf8_lossy(&human)
                    .contains("identity and geometry supplied by validated bootstrap"));
            }
        }
    }

    /// Complete recovery hints cannot override a validated compression refusal.
    #[test]
    fn recovery_report_refuses_compressed_bootstrap() {
        let mut files = plaintext_image(true);
        let block = &mut files[0];
        let end = 56 + u32::from_le_bytes(block[44..48].try_into().unwrap()) as usize;
        assert_eq!(&block[end - 2..end], &[5, 0xf4]);
        block[end - 1] = 0xf5;
        let crc = remanence_parity::crc64_xz(&block[56..end]);
        block[end..end + 8].copy_from_slice(&crc.to_le_bytes());
        let temp = write_image_directory(&files);
        let mut source = ImageDirectoryRawSource::open(temp.path()).expect("image");
        let error = build_recovery_report(
            &mut source,
            &[BLOCK_SIZE],
            Some(RecoveryHints {
                tape_uuid: uuid::Uuid::from_bytes(TAPE_UUID).to_string(),
                block_size_bytes: BLOCK_SIZE,
                scheme: "128,4,64".to_string(),
            }),
        )
        .expect_err("compression refusal");
        assert!(
            error.contains(&ParityError::DriveCompressionEnabled.to_string()),
            "{error}"
        );
    }

    /// Header evidence remains binding when a later payload or schema check fails.
    #[test]
    fn recovery_report_refuses_checksummed_header_conflicts() {
        for field in ["identity", "block size"] {
            for failure in ["payload", "schema"] {
                let mut files = plaintext_image(false);
                let block = &mut files[0];
                if field == "identity" {
                    block[16] ^= 1;
                } else {
                    block[32..36].copy_from_slice(&(BLOCK_SIZE / 2).to_be_bytes());
                }
                if failure == "payload" {
                    block[80] ^= 1;
                } else {
                    block[8..10].copy_from_slice(&99u16.to_be_bytes());
                }
                let crc = remanence_parity::crc64_xz(&block[..48]);
                block[48..56].copy_from_slice(&crc.to_le_bytes());
                let temp = write_image_directory(&files);
                let mut source = ImageDirectoryRawSource::open(temp.path()).expect("image");
                let error = build_recovery_report(
                    &mut source,
                    &[BLOCK_SIZE],
                    Some(RecoveryHints {
                        tape_uuid: uuid::Uuid::from_bytes(TAPE_UUID).to_string(),
                        block_size_bytes: BLOCK_SIZE,
                        scheme: "none".to_string(),
                    }),
                )
                .expect_err("checksummed header conflict");
                let reason = if failure == "schema" {
                    "got 99, accept 2"
                } else {
                    field
                };
                assert!(error.contains(reason), "{error}");
                assert!(
                    error.starts_with("discover bootstrap:"),
                    "refuse during discovery: {error}"
                );
            }
        }
    }

    /// Discovery refuses valid header disagreements before inspecting damaged payloads.
    #[test]
    fn recovery_report_refuses_checked_format_fields() {
        for (offset, bytes, reason) in [
            (8, 1u16.to_be_bytes().to_vec(), "got 1, accept 2"),
            (8, 3u16.to_be_bytes().to_vec(), "got 3, accept 2"),
            (36, 7u64.to_be_bytes().to_vec(), "got sequence 7"),
            (12, 0u32.to_be_bytes().to_vec(), "no-parity flag"),
        ] {
            for damaged_payload in [false, true] {
                let mut files = plaintext_image(false);
                let block = &mut files[0];
                block[offset..offset + bytes.len()].copy_from_slice(&bytes);
                if damaged_payload {
                    block[80] ^= 1;
                }
                let crc = remanence_parity::crc64_xz(&block[..48]);
                block[48..56].copy_from_slice(&crc.to_le_bytes());
                let temp = write_image_directory(&files);
                let mut source = ImageDirectoryRawSource::open(temp.path()).expect("image");
                let error = build_recovery_report(
                    &mut source,
                    &[BLOCK_SIZE],
                    Some(RecoveryHints {
                        tape_uuid: uuid::Uuid::from_bytes(TAPE_UUID).to_string(),
                        block_size_bytes: BLOCK_SIZE,
                        scheme: "none".to_string(),
                    }),
                )
                .expect_err("checked header refusal");
                assert!(error.contains(reason), "{error}");
                assert!(error.starts_with("discover bootstrap:"), "{error}");
            }
        }
    }

    /// A successful candidate read cannot hide a disagreement with hinted geometry.
    #[test]
    fn recovery_report_refuses_header_geometry_from_another_candidate_size() {
        let files = plaintext_image(false);
        let temp = write_image_directory(&files);
        let mut source = ImageDirectoryRawSource::open(temp.path()).expect("image");
        let error = build_recovery_report(
            &mut source,
            &[BLOCK_SIZE],
            Some(RecoveryHints {
                tape_uuid: uuid::Uuid::from_bytes(TAPE_UUID).to_string(),
                block_size_bytes: BLOCK_SIZE * 2,
                scheme: "none".to_string(),
            }),
        )
        .expect_err("candidate header disagrees with hints");
        assert!(
            error.contains("readable bootstrap block size differs from supplied hints"),
            "{error}"
        );
        assert!(error.starts_with("discover bootstrap:"), "{error}");
    }

    /// Partial authority and invalid geometry must fail before touching the source.
    #[test]
    fn recovery_report_cli_rejects_partial_or_invalid_hints() {
        let flags = [
            ["--tape-uuid", "42424242-4242-4242-4242-424242424242"],
            ["--block-size", "262144"],
            ["--scheme", "none"],
        ];
        for mask in 1..7 {
            let mut args = vec!["rem-debug", "tape", "recovery-report", "image"];
            for (bit, flag) in flags.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    args.extend(flag);
                }
            }
            assert!(
                crate::DebugCli::try_parse_from(args).is_err(),
                "partial mask {mask}"
            );
        }
        for scheme in [
            "default",
            "custom:128,4,1",
            "0,4,1",
            "128,0,1",
            "128,4,0",
            "128,4",
            "none,1,1",
        ] {
            assert!(parse_scheme(scheme).is_err(), "invalid scheme {scheme}");
        }
        assert!(parse_tape_uuid("invalid").is_err());
        assert!(parse_block_size("0").is_err());
    }

    fn parity_scheme_record() -> ParitySchemeRecord {
        let scheme = default_scheme_for_block_size(BLOCK_SIZE);
        ParitySchemeRecord {
            id: scheme.id.as_str().to_string(),
            data_blocks_per_stripe: scheme.data_blocks_per_stripe,
            parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
            stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            no_parity_flag: false,
        }
    }

    fn bootstrap_block(parity_protected: bool) -> Vec<u8> {
        let map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT map validates");
        let payload = BootstrapPayload {
            scheme: parity_protected.then(parity_scheme_record),
            no_parity_flag: !parity_protected,
            filemark_map_digest: Some(map.digest(false).expect("BOT map digest builds")),
            tape_uuid: TAPE_UUID,
            written_by_version: "recovery-report-hermetic-test".to_string(),
            written_at: "2026-08-09T00:00:00Z".to_string(),
            sequence: 0,
            block_size_bytes: BLOCK_SIZE,
            drive_compression: false,
        };
        let mut block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&payload, &mut block).expect("major-2 bootstrap block encodes");
        block
    }

    #[derive(Clone)]
    struct TerminalRows {
        entries: Vec<TapeIndexReplicaMapEntry>,
        rows: Vec<TapeIndexReplicaObjectRow>,
    }

    impl TapeIndexReplicaRecordSource for TerminalRows {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for entry in &self.entries {
                visitor(entry)?;
            }
            Ok(())
        }

        fn visit_object_rows(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for row in &self.rows {
                visitor(row)?;
            }
            Ok(())
        }
    }

    fn minimal_image() -> Vec<Vec<u8>> {
        terminal_image(None, false)
    }

    fn plaintext_image(parity_protected: bool) -> Vec<Vec<u8>> {
        let manifest = b"hermetic major-2 plaintext manifest";
        let mut object = vec![0x31; BLOCK_SIZE as usize * 2];
        object[BLOCK_SIZE as usize..BLOCK_SIZE as usize + manifest.len()].copy_from_slice(manifest);
        let manifest_digest: [u8; 32] = Sha256::digest(manifest).into();
        let row = TapeIndexReplicaObjectRow {
            tape_file_number: 1,
            stored_block_count: 2,
            object_id: OBJECT_ID.as_bytes().to_vec(),
            representation: ObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba: 1,
                manifest_size_bytes: manifest.len() as u64,
                manifest_chunk_count: 1,
                manifest_sha256: manifest_digest,
            },
        };
        image_with_object(object, row, parity_protected)
    }

    fn sealed_object() -> (Vec<u8>, SealReport, Vec<[u8; 16]>) {
        let recipient_epoch_id = [0x71; 16];
        let recipient = RecipientPrivateKey::new(recipient_epoch_id, "hermetic-recovery", [7; 32])
            .expect("hermetic recipient key builds");
        let plaintext = vec![0x5a; BLOCK_SIZE as usize];
        let plaintext_digest: [u8; 32] = Sha256::digest(&plaintext).into();
        let options = EnvelopeSealOptions {
            common: SealOptions {
                chunk_size: BLOCK_SIZE,
                object_id: OBJECT_ID.to_string(),
                plaintext_size: plaintext.len() as u64,
                plaintext_digest,
            },
            allow_single_recipient: true,
            recipients: vec![recipient
                .public_key(0)
                .expect("recipient public key derives")],
        };
        let mut object = Vec::new();
        let report = seal_deterministic_for_test_vectors(
            plaintext.as_slice(),
            &mut object,
            &options,
            DataEncryptionKey::from_bytes([0x44; 32]),
            [0x55; 32],
        )
        .expect("object seals deterministically");
        (object, report, vec![recipient_epoch_id])
    }

    fn encrypted_image() -> Vec<Vec<u8>> {
        let (object, seal, recipient_epoch_ids) = sealed_object();
        let row = TapeIndexReplicaObjectRow {
            tape_file_number: 1,
            stored_block_count: seal.stored_size_blocks,
            object_id: OBJECT_ID.as_bytes().to_vec(),
            representation: ObjectRecoveryRepresentation::Encrypted {
                recipient_epoch_ids,
                metadata_frame_len: seal.metadata_frame_len,
                key_frame_len: seal.header.key_frame_len,
            },
        };
        image_with_object(object, row, true)
    }

    fn image_with_object(
        object: Vec<u8>,
        row: TapeIndexReplicaObjectRow,
        parity_protected: bool,
    ) -> Vec<Vec<u8>> {
        terminal_image(Some((object, row)), parity_protected)
    }

    fn terminal_image(
        object: Option<(Vec<u8>, TapeIndexReplicaObjectRow)>,
        parity_protected: bool,
    ) -> Vec<Vec<u8>> {
        let mut entries = vec![TapeIndexReplicaMapEntry {
            tape_file_number: 0,
            kind: TapeIndexReplicaFileKind::Bootstrap,
            block_count: 1,
            first_parity_data_ordinal: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        }];
        let mut rows = Vec::new();
        let mut files = vec![bootstrap_block(parity_protected)];
        if let Some((object, row)) = object {
            entries.push(TapeIndexReplicaMapEntry {
                tape_file_number: 1,
                kind: TapeIndexReplicaFileKind::Object,
                block_count: row.stored_block_count,
                first_parity_data_ordinal: Some(0),
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            });
            rows.push(row);
            files.push(object);
        }
        let authority = TerminalRows { entries, rows };
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: authority.entries.len() as u64,
            object_row_count: authority.rows.len() as u64,
        };
        let replica_records = checked_tape_index_replica_layout(BLOCK_SIZE, counts)
            .expect("terminal replica layout")
            .replica_record_count;
        let prefix_end_lba = authority
            .entries
            .iter()
            .map(|entry| entry.block_count + 1)
            .sum();
        let layout = TerminalTailLayout::new(
            0,
            BLOCK_SIZE,
            counts.structural_entry_count,
            prefix_end_lba,
            replica_records,
            3,
        )
        .expect("terminal tail layout");
        let mut planning = authority.clone();
        let edition = plan_tape_index_edition(
            TapeIndexEditionDescriptor {
                tape_uuid: TAPE_UUID,
                edition_id: [0x52; 16],
                edition_sequence: 1,
                scope: TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: counts.structural_entry_count,
                    total_data_ordinals: authority
                        .entries
                        .iter()
                        .filter(|entry| entry.kind == TapeIndexReplicaFileKind::Object)
                        .map(|entry| entry.block_count)
                        .sum(),
                    highest_protected_ordinal: 0,
                },
                counts,
                block_size: BLOCK_SIZE,
                compression_enabled: false,
                writer_version: "recovery-report-hermetic-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: layout,
            },
            &mut planning,
        )
        .expect("terminal edition plans");
        for ordinal in 1..=3 {
            let plan = plan_tape_index_replica(edition.clone(), ordinal).expect("replica plan");
            let mut file = Vec::new();
            let mut source = authority.clone();
            write_tape_index_replica(
                &plan,
                TapeIndexReplicaObservation {
                    tape_file_number: plan.component.planned_tape_file_number,
                    start_lba: plan.component.planned_start_lba,
                    record_count: plan.component.record_count,
                },
                &mut source,
                |block| {
                    file.extend_from_slice(block);
                    Ok(())
                },
            )
            .expect("terminal replica writes");
            files.push(file);
            if ordinal != 3 {
                let separation = plan_index_separation(IndexSeparationDescriptor {
                    tape_uuid: TAPE_UUID,
                    edition_id: edition.descriptor.edition_id,
                    gap_ordinal: ordinal,
                    block_size: BLOCK_SIZE,
                    nominal_extent_bytes: 3 * u64::from(BLOCK_SIZE),
                    total_records: 3,
                    compression_enabled: false,
                    terminal_layout: layout,
                })
                .expect("separation plan");
                let mut gap = Vec::new();
                write_index_separation(
                    &separation,
                    IndexSeparationObservation {
                        tape_file_number: separation.component.planned_tape_file_number,
                        start_lba: separation.component.planned_start_lba,
                        record_count: separation.component.record_count,
                    },
                    |block| {
                        gap.extend_from_slice(block);
                        Ok(())
                    },
                )
                .expect("separation writes");
                files.push(gap);
            }
        }
        files
    }

    fn write_image_directory(tape_files: &[Vec<u8>]) -> TempDir {
        let temp = tempfile::tempdir().expect("hermetic image directory creates");
        for (index, bytes) in tape_files.iter().enumerate() {
            let kind = if index == 0 {
                "bootstrap"
            } else if index == 1 && tape_files.len() == 7 {
                "object"
            } else {
                "terminal"
            };
            fs::write(
                temp.path().join(format!("tape-file-{index:03}-{kind}.bin")),
                bytes,
            )
            .expect("hermetic tape file writes");
        }
        temp
    }

    fn image_report(path: &Path) -> CatalogLessRecoveryReport {
        let mut source =
            ImageDirectoryRawSource::open(path).expect("hermetic image directory opens");
        let candidates = source.candidate_block_sizes().to_vec();
        build_recovery_report(&mut source, &candidates, None).expect("hermetic image report builds")
    }

    #[test]
    fn minimal_image_json_stable_fields_are_green() {
        let temp = write_image_directory(&minimal_image());
        let report = image_report(temp.path());
        let actual = serde_json::to_value(report).expect("report serializes");
        assert_eq!(
            json!({
                "report_version": actual["report_version"],
                "tape_uuid": actual["tape_uuid"],
                "block_size_bytes": actual["block_size_bytes"],
                "scan": {
                    "bootstrap_generation_used": actual["scan"]["bootstrap_generation_used"],
                    "overlay_source": actual["scan"]["overlay_source"],
                    "recovered_scope_tape_file_count": actual["scan"]["recovered_scope_tape_file_count"],
                    "damaged_regions": actual["scan"]["damaged_regions"],
                },
                "totals": actual["totals"],
                "success": actual["success"],
            }),
            json!({
                "report_version": 1,
                "tape_uuid": "42424242424242424242424242424242",
                "block_size_bytes": 262144,
                "scan": {
                    "bootstrap_generation_used": "0",
                    "overlay_source": "terminal_index_replica_c",
                    "recovered_scope_tape_file_count": "1",
                    "damaged_regions": [],
                },
                "totals": {
                    "objects_seen": "0",
                    "map_agreeing": "0",
                    "verified": "0",
                    "failed": "0",
                    "beyond_scope": "0",
                },
                "success": true,
            })
        );
    }

    #[test]
    fn recovery_report_json_preserves_structural_u64_values_as_decimal_strings() {
        let report = CatalogLessRecoveryReport {
            report_version: 1,
            tape_uuid: Some("42424242424242424242424242424242".to_string()),
            block_size_bytes: Some(BLOCK_SIZE),
            supplied: None,
            scan: RecoveryScanSummary {
                bootstrap_treated_as_unreadable: false,
                identity_and_geometry_from_hints: false,
                bootstrap_generation_used: Some(u64::MAX),
                bootstrap_tape_file_number: Some(u64::MAX),
                overlay_source: "structural_walk",
                recovered_scope_tape_file_count: u64::MAX,
                damaged_regions: vec![RecoveryDamageRegion {
                    start_lba: u64::MAX,
                    partition: 0,
                    block_count: u64::MAX,
                    kind: "unreadable_tape_file_head",
                }],
                truncation: Some(RecoveryTruncation {
                    tape_file_number: u64::MAX,
                    start_lba: u64::MAX,
                    partition: 0,
                    kind: "missing_trailing_filemark",
                }),
            },
            objects: vec![RecoveryObjectReport {
                tape_file_number: u64::MAX,
                representation: "plaintext",
                object_id: Value::String(OBJECT_ID.to_string()),
                object_id_encoding: "utf8",
                object_id_human: OBJECT_ID.to_string(),
                stored_block_count: u64::MAX,
                map_status: "map_agrees",
                map_detail: None,
                verification_status: "manifest_verified",
                verification_detail: None,
            }],
            totals: RecoveryTotals {
                objects_seen: u64::MAX,
                map_agreeing: u64::MAX,
                verified: u64::MAX,
                failed: u64::MAX,
                beyond_scope: u64::MAX,
            },
            success: true,
        };

        let json = serde_json::to_value(report).expect("recovery report serializes");
        let maximum = Value::String(u64::MAX.to_string());
        for pointer in [
            "/scan/bootstrap_generation_used",
            "/scan/bootstrap_tape_file_number",
            "/scan/recovered_scope_tape_file_count",
            "/scan/damaged_regions/0/start_lba",
            "/scan/damaged_regions/0/block_count",
            "/scan/truncation/tape_file_number",
            "/scan/truncation/start_lba",
            "/objects/0/tape_file_number",
            "/objects/0/stored_block_count",
            "/totals/objects_seen",
            "/totals/map_agreeing",
            "/totals/verified",
            "/totals/failed",
            "/totals/beyond_scope",
        ] {
            assert_eq!(json.pointer(pointer), Some(&maximum), "{pointer}");
        }
    }

    #[test]
    fn plaintext_major_2_image_verifies_manifest_and_object_identity() {
        let temp = write_image_directory(&plaintext_image(false));
        let report = image_report(temp.path());
        assert!(report.success);
        assert_eq!(report.totals.objects_seen, 1);
        assert_eq!(report.totals.map_agreeing, 1);
        assert_eq!(report.totals.verified, 1);
        let object = &report.objects[0];
        assert_eq!(object.map_status, "map_agrees");
        assert_eq!(object.verification_status, "manifest_verified");
        assert_eq!(object.object_id, Value::String(OBJECT_ID.to_string()));
        assert_eq!(object.object_id_encoding, "utf8");
    }

    fn assert_attested_major_2_image(
        image: Vec<Vec<u8>>,
        representation: &str,
        verification_status: &str,
    ) {
        let temp = write_image_directory(&image);
        let report = image_report(temp.path());
        assert!(report.success);
        assert_eq!(report.scan.bootstrap_generation_used, Some(0));
        assert_eq!(report.scan.bootstrap_tape_file_number, Some(0));
        assert_eq!(report.scan.overlay_source, "terminal_index_replica_c");
        assert_eq!(report.scan.recovered_scope_tape_file_count, 2);
        assert!(report.scan.damaged_regions.is_empty());
        assert_eq!(report.totals.objects_seen, 1);
        assert_eq!(report.totals.map_agreeing, 1);
        assert_eq!(report.totals.verified, 1);
        assert_eq!(report.totals.failed, 0);
        let object = &report.objects[0];
        assert_eq!(object.representation, representation);
        assert_eq!(object.map_status, "map_agrees");
        assert_eq!(object.verification_status, verification_status);
        assert_eq!(object.object_id, Value::String(OBJECT_ID.to_string()));
    }

    #[test]
    fn parity_protected_plaintext_major_2_image_is_attested_and_green() {
        assert_attested_major_2_image(plaintext_image(true), "plaintext", "manifest_verified");
    }

    #[test]
    fn encrypted_major_2_image_is_attested_and_green() {
        assert_attested_major_2_image(encrypted_image(), "encrypted", "envelope_consistent");
    }

    #[test]
    fn rows_outside_scope_are_not_failures_and_in_scope_map_mismatches_are() {
        let above_u32 = u64::from(u32::MAX) + 17;
        let row = TapeIndexReplicaObjectRow {
            tape_file_number: above_u32,
            stored_block_count: 2,
            object_id: b"outside-scope".to_vec(),
            representation: ObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba: 0,
                manifest_size_bytes: 1,
                manifest_chunk_count: 1,
                manifest_sha256: [0u8; 32],
            },
        };
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 1, 0),
        ])
        .expect("one-object map is valid");
        let mut source =
            ImageDirectoryRawSource::from_tape_files(vec![vec![0u8; 4096], vec![0u8; 4096]], 4096)
                .expect("BOT-plus-object image wraps");

        let beyond = report_object_row(&mut source, &map, 2, 4096, &row);
        assert_eq!(beyond.tape_file_number, above_u32);
        assert_eq!(beyond.map_status, "beyond_recovered_scope");
        assert_eq!(beyond.verification_status, "not_checked_beyond_scope");

        let mismatching_row = TapeIndexReplicaObjectRow {
            tape_file_number: 1,
            stored_block_count: 2,
            object_id: b"mismatch".to_vec(),
            representation: ObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba: 0,
                manifest_size_bytes: 1,
                manifest_chunk_count: 1,
                manifest_sha256: [0u8; 32],
            },
        };
        let mismatch = report_object_row(&mut source, &map, 2, 4096, &mismatching_row);
        assert_eq!(mismatch.map_status, "map_mismatch");
        assert!(mismatch
            .map_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("stored block count mismatch")));
        assert_eq!(mismatch.verification_status, "not_checked");
    }

    #[test]
    fn unreadable_manifest_block_is_a_precise_row_failure_and_exit_two() {
        let temp = write_image_directory(&plaintext_image(false));
        let mut source =
            ImageDirectoryRawSource::open(temp.path()).expect("hermetic plaintext image opens");
        source
            .mark_unreadable(1, 1)
            .expect("hermetic manifest block exists");
        let candidates = source.candidate_block_sizes().to_vec();
        let report = build_recovery_report(&mut source, &candidates, None)
            .expect("damage remains reportable");
        assert!(!report.success);
        assert_eq!(report.totals.failed, 1);
        assert_eq!(
            report.objects[0].verification_status,
            "manifest_unreadable_blocks"
        );
        assert!(report.objects[0]
            .verification_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("block 1")));

        let mut source =
            ImageDirectoryRawSource::open(temp.path()).expect("hermetic plaintext image reopens");
        source
            .mark_unreadable(1, 1)
            .expect("hermetic manifest block exists");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run_raw_recovery_report(
            &mut source,
            &candidates,
            None,
            true,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::from(2));
        assert!(stderr.is_empty());
    }

    #[test]
    fn manifest_byte_damage_is_digest_mismatch_and_exit_two() {
        let temp = write_image_directory(&plaintext_image(false));
        let object_path = temp.path().join("tape-file-001-object.bin");
        let mut object = fs::read(&object_path).expect("hermetic image object reads");
        object[BLOCK_SIZE as usize] ^= 1;
        fs::write(&object_path, object)
            .expect("fault injection rewrites only the hermetic temporary image");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run_image_recovery_report(temp.path(), None, true, &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::from(2));
        assert!(stderr.is_empty());
        let report: Value = serde_json::from_slice(&stdout).expect("JSON failure report parses");
        assert_eq!(
            report["objects"][0]["verification_status"],
            "manifest_digest_mismatch"
        );
        assert_eq!(report["totals"]["failed"], "1");
    }

    #[test]
    fn encrypted_hermetic_object_exercises_key_21_through_23_checks() {
        let (object, seal, recipient_epoch_ids) = sealed_object();
        let row = TapeIndexReplicaObjectRow {
            tape_file_number: 1,
            stored_block_count: seal.stored_size_blocks,
            object_id: OBJECT_ID.as_bytes().to_vec(),
            representation: ObjectRecoveryRepresentation::Encrypted {
                recipient_epoch_ids: recipient_epoch_ids.clone(),
                metadata_frame_len: seal.metadata_frame_len,
                key_frame_len: seal.header.key_frame_len,
            },
        };
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, seal.stored_size_blocks, 0),
        ])
        .expect("one-object map is valid");
        let mut source = ImageDirectoryRawSource::from_tape_files(
            vec![vec![0u8; BLOCK_SIZE as usize], object.clone()],
            BLOCK_SIZE,
        )
        .expect("hermetic encrypted object wraps after BOT");
        source
            .configure_fixed_block_size(BLOCK_SIZE)
            .expect("hermetic encrypted image configures");
        let verification = verify_encrypted_envelope(
            &mut source,
            &map,
            &row,
            BLOCK_SIZE,
            &recipient_epoch_ids,
            seal.metadata_frame_len,
            seal.header.key_frame_len,
        );
        assert_eq!(verification.status, "envelope_consistent");
        assert_eq!(verification.detail, None);

        let footer_offset = object
            .windows(REM_OBJECT_FOOTER.len())
            .rposition(|window| window == REM_OBJECT_FOOTER)
            .expect("hermetic encrypted object carries completion footer");
        let mut damaged_object = object;
        damaged_object[footer_offset] ^= 1;
        let mut damaged_source = ImageDirectoryRawSource::from_tape_files(
            vec![vec![0u8; BLOCK_SIZE as usize], damaged_object],
            BLOCK_SIZE,
        )
        .expect("damaged encrypted object wraps after BOT");
        damaged_source
            .configure_fixed_block_size(BLOCK_SIZE)
            .expect("damaged encrypted image configures");
        let damaged = verify_encrypted_envelope(
            &mut damaged_source,
            &map,
            &row,
            BLOCK_SIZE,
            &recipient_epoch_ids,
            seal.metadata_frame_len,
            seal.header.key_frame_len,
        );
        assert_eq!(damaged.status, "envelope_completion_invalid");
    }

    #[test]
    fn non_utf8_object_identity_is_base64_in_json_and_lossy_for_humans() {
        let bytes = vec![0xff, b'A'];
        let (json_value, encoding, human) = render_object_id(Some(bytes.as_slice()));
        assert_eq!(json_value, Value::String(BASE64_STANDARD.encode(bytes)));
        assert_eq!(encoding, "base64");
        assert!(human.contains('\u{fffd}'));
    }

    #[test]
    fn absent_legacy_object_identity_uses_the_required_marker() {
        let (json_value, encoding, human) = render_object_id(None);
        assert_eq!(json_value, Value::String("absent(minor<=2)".to_string()));
        assert_eq!(encoding, "absent");
        assert_eq!(human, "absent(minor<=2)");
    }

    #[test]
    fn human_output_failure_is_operational_exit_one() {
        struct FailingWriter;

        impl std::io::Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("synthetic output failure"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let temp = write_image_directory(&minimal_image());
        let mut source =
            ImageDirectoryRawSource::open(temp.path()).expect("hermetic minimal image opens");
        let candidates = source.candidate_block_sizes().to_vec();
        let mut stdout = FailingWriter;
        let mut stderr = Vec::new();
        let exit = run_raw_recovery_report(
            &mut source,
            &candidates,
            None,
            false,
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(exit, ExitCode::from(1));
        assert!(String::from_utf8(stderr)
            .expect("error is UTF-8")
            .contains("write human recovery report"));
    }
}
