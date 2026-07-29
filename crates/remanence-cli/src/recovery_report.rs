//! Catalog-less key-30 recovery reporting for `rem-debug tape`.
//!
//! This module composes the Layer 3c structural scan, authoritative-bootstrap
//! selection, directory overlay, and digest-validation pipeline with bounded
//! plaintext-manifest and keyless REM-ENCRYPT checks. It never consults the
//! catalog, daemon, or host persistence.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use remanence_aead::{KeyFrame, RemObjectHeader, REM_OBJECT_FOOTER, REM_OBJECT_HEADER_LEN};
use remanence_parity::{
    discover_bootstrap_with_candidate_block_sizes, scan_reconstruct_filemark_map_with_report,
    validate_scan_reconstruction_with_report, BootstrapObjectRepresentation, BootstrapObjectRow,
    FilemarkMap, ImageDirectoryRawSource, RawReadOutcome, RawTapeSource, ScanDamageKind,
    ScanDamagedRegion, ScanOverlaySource, ScanTailTruncation, ScanTailTruncationKind, TapeFileKind,
    TapeFilePosition,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct CatalogLessRecoveryReport {
    report_version: u32,
    tape_uuid: String,
    block_size_bytes: u32,
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
    bootstrap_generation_used: u32,
    bootstrap_tape_file_number: Option<u32>,
    overlay_source: &'static str,
    recovered_scope_tape_file_count: u32,
    damaged_regions: Vec<RecoveryDamageRegion>,
    truncation: Option<RecoveryTruncation>,
}

#[derive(Clone, Debug, Serialize)]
struct RecoveryDamageRegion {
    start_lba: u64,
    partition: u32,
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
            },
        }
    }
}

#[derive(Clone, Debug, Serialize)]
struct RecoveryTruncation {
    tape_file_number: u32,
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
    tape_file_number: u32,
    representation: &'static str,
    object_id: Value,
    object_id_encoding: &'static str,
    #[serde(skip)]
    object_id_human: String,
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
    objects_seen: u64,
    map_agreeing: u64,
    verified: u64,
    failed: u64,
    beyond_scope: u64,
}

struct RecoveredMap {
    map: FilemarkMap,
    scope_tape_file_count: u32,
    bootstrap_generation_used: u32,
    overlay_source: &'static str,
    damaged_regions: Vec<ScanDamagedRegion>,
    truncation: Option<ScanTailTruncation>,
}

/// Run a report against a published-layout image directory.
pub(crate) fn run_image_recovery_report(
    image_directory: &Path,
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
    run_raw_recovery_report(&mut source, &candidate_block_sizes, json_output, out, err)
}

/// Run a report against any production Layer 3c raw source.
pub(crate) fn run_raw_recovery_report(
    source: &mut dyn RawTapeSource,
    candidate_block_sizes: &[u32],
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let report = match build_recovery_report(source, candidate_block_sizes) {
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
) -> Result<CatalogLessRecoveryReport, String> {
    if candidate_block_sizes.is_empty() {
        return Err("no candidate fixed-block sizes were supplied".to_string());
    }
    let first_bootstrap =
        discover_bootstrap_with_candidate_block_sizes(source, None, candidate_block_sizes)
            .map_err(|error| format!("discover bootstrap: {error}"))?;
    let scan = scan_reconstruct_filemark_map_with_report(
        source,
        &first_bootstrap.tape_uuid,
        first_bootstrap.block_size_bytes,
    )
    .map_err(|error| format!("scan filemark map: {error}"))?;
    let winning_candidate = scan.authoritative_bootstrap().cloned();
    let winning_bootstrap = winning_candidate.as_ref().map_or_else(
        || first_bootstrap.clone(),
        |candidate| candidate.payload.clone(),
    );
    let winning_tape_file_number = winning_candidate
        .as_ref()
        .map(|candidate| candidate.tape_file_number);

    let recovered = if winning_bootstrap.filemark_map_digest.is_some() {
        let validated = validate_scan_reconstruction_with_report(source, &winning_bootstrap, scan)
            .map_err(|error| format!("validate recovered filemark map: {error}"))?;
        RecoveredMap {
            scope_tape_file_count: validated.attested_map.tape_file_count(),
            map: validated.scoped_map.map,
            bootstrap_generation_used: validated.authoritative_bootstrap_sequence,
            overlay_source: overlay_source_name(validated.overlay_source),
            damaged_regions: validated.damaged_regions,
            truncation: validated.truncation,
        }
    } else if winning_bootstrap.no_parity_flag {
        RecoveredMap {
            scope_tape_file_count: scan.map.tape_file_count(),
            map: scan.map,
            bootstrap_generation_used: winning_bootstrap.sequence,
            overlay_source: "structural_walk_no_digest",
            damaged_regions: scan.damaged_regions,
            truncation: scan.truncation,
        }
    } else {
        return Err("parity bootstrap has no filemark-map digest".to_string());
    };

    let mut totals = RecoveryTotals {
        objects_seen: u64::try_from(winning_bootstrap.object_rows.len())
            .map_err(|_| "object-row count exceeds u64::MAX".to_string())?,
        ..RecoveryTotals::default()
    };
    let mut objects = Vec::with_capacity(winning_bootstrap.object_rows.len());
    for row in &winning_bootstrap.object_rows {
        let object = report_object_row(
            source,
            &recovered.map,
            recovered.scope_tape_file_count,
            winning_bootstrap.block_size_bytes,
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

    Ok(CatalogLessRecoveryReport {
        report_version: 1,
        tape_uuid: hex(&winning_bootstrap.tape_uuid),
        block_size_bytes: winning_bootstrap.block_size_bytes,
        scan: RecoveryScanSummary {
            bootstrap_generation_used: recovered.bootstrap_generation_used,
            bootstrap_tape_file_number: winning_tape_file_number,
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

fn report_object_row(
    source: &mut dyn RawTapeSource,
    map: &FilemarkMap,
    recovered_scope_tape_file_count: u32,
    block_size: u32,
    row: &BootstrapObjectRow,
) -> RecoveryObjectReport {
    let (object_id, object_id_encoding, object_id_human) = render_object_id(&row.object_id);
    let representation = match row.representation {
        BootstrapObjectRepresentation::Plaintext { .. } => "plaintext",
        BootstrapObjectRepresentation::Encrypted { .. } => "encrypted",
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

    let Some(entry) = map
        .entries()
        .get(usize::try_from(row.tape_file_number).unwrap_or(usize::MAX))
    else {
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
        BootstrapObjectRepresentation::Plaintext {
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
        BootstrapObjectRepresentation::Encrypted {
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
    row: &BootstrapObjectRow,
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
    row: &BootstrapObjectRow,
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
    tape_file_number: u32,
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
    tape_file_number: u32,
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
    tape_file_number: u32,
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

fn render_object_id(object_id: &Option<Vec<u8>>) -> (Value, &'static str, String) {
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

fn overlay_source_name(source: ScanOverlaySource) -> &'static str {
    match source {
        ScanOverlaySource::StructuralWalk => "structural_walk",
        ScanOverlaySource::Catalog => "catalog",
        ScanOverlaySource::BootstrapInlineDirectory => "bootstrap_inline_directory",
        ScanOverlaySource::ReferencedParityMap => "referenced_parity_map",
        ScanOverlaySource::StructurallySelectedParityMap => "structurally_selected_parity_map",
        ScanOverlaySource::ParityMapReferenceProjection => "parity_map_reference_projection",
    }
}

fn print_human_report(
    report: &CatalogLessRecoveryReport,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    writeln!(out, "catalog-less recovery report")?;
    writeln!(out, "tape uuid: {}", report.tape_uuid)?;
    writeln!(out, "block size: {} bytes", report.block_size_bytes)?;
    writeln!(
        out,
        "scan: bootstrap generation {} tape-file {} overlay {} damaged-regions {} scope-files {}",
        report.scan.bootstrap_generation_used,
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
    use std::process::Command as ProcessCommand;

    use clap::Parser as _;
    use remanence_parity::{BootstrapObjectRow, FilemarkMap, RawTapeSource, TapeFileMapEntry};
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    const OBJECT_ID_IMAGE: &str = "rem-parity-1/positive/object-id-36-bootstrap";
    const MINIMAL_IMAGE: &str = "rem-parity-1/positive/minimal-image";
    const KEY_30_PLAINTEXT_IMAGE: &str = "rem-parity-1/positive/key-30-plaintext-attested";
    const KEY_30_ENCRYPTED_IMAGE: &str = "rem-parity-1/positive/key-30-encrypted-attested";
    const ENCRYPTED_OBJECT: &str = "rem-object/objects/rem-object-tv-e2.rem-object";
    const ENCRYPTED_MANIFEST: &str = "rem-object/manifests/rem-object-tv-e2.json";

    fn archive_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../specs/publication/remanence-test-vectors.tar")
    }

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

    fn extract_archive_entries(entries: &[&str]) -> TempDir {
        let temp = tempfile::tempdir().expect("fixture extraction directory creates");
        let mut command = ProcessCommand::new("tar");
        command
            .arg("-xf")
            .arg(archive_path())
            .arg("-C")
            .arg(temp.path())
            .args(entries);
        let status = command
            .status()
            .expect("tar is required to extract the pinned publication archive");
        assert!(
            status.success(),
            "tar must extract every requested pinned publication fixture"
        );
        temp
    }

    fn image_report(path: &Path) -> CatalogLessRecoveryReport {
        let mut source = ImageDirectoryRawSource::open(path).expect("pinned image directory opens");
        let candidates = source.candidate_block_sizes().to_vec();
        build_recovery_report(&mut source, &candidates).expect("pinned image report builds")
    }

    #[test]
    fn minimal_image_json_stable_fields_are_green() {
        let temp = extract_archive_entries(&[MINIMAL_IMAGE]);
        let report = image_report(&temp.path().join(MINIMAL_IMAGE));
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
                "block_size_bytes": 4096,
                "scan": {
                    "bootstrap_generation_used": 1,
                    "overlay_source": "bootstrap_inline_directory",
                    "recovered_scope_tape_file_count": 4,
                    "damaged_regions": [],
                },
                "totals": {
                    "objects_seen": 0,
                    "map_agreeing": 0,
                    "verified": 0,
                    "failed": 0,
                    "beyond_scope": 0,
                },
                "success": true,
            })
        );
    }

    #[test]
    fn plaintext_key_30_image_verifies_manifest_and_object_identity() {
        let temp = extract_archive_entries(&[OBJECT_ID_IMAGE]);
        let report = image_report(&temp.path().join(OBJECT_ID_IMAGE));
        assert!(report.success);
        assert_eq!(report.totals.objects_seen, 1);
        assert_eq!(report.totals.map_agreeing, 1);
        assert_eq!(report.totals.verified, 1);
        let object = &report.objects[0];
        assert_eq!(object.map_status, "map_agrees");
        assert_eq!(object.verification_status, "manifest_verified");
        assert_eq!(
            object.object_id,
            Value::String("00000000-0000-4000-8000-000000000001".to_string())
        );
        assert_eq!(object.object_id_encoding, "utf8");
    }

    fn assert_attested_key_30_image(image: &str, representation: &str, verification_status: &str) {
        let temp = extract_archive_entries(&[image]);
        let report = image_report(&temp.path().join(image));
        assert!(report.success);
        assert_eq!(report.scan.bootstrap_generation_used, 1);
        assert_eq!(report.scan.bootstrap_tape_file_number, Some(3));
        assert_eq!(report.scan.overlay_source, "bootstrap_inline_directory");
        assert_eq!(report.scan.recovered_scope_tape_file_count, 4);
        assert!(report.scan.damaged_regions.is_empty());
        assert_eq!(report.totals.objects_seen, 1);
        assert_eq!(report.totals.map_agreeing, 1);
        assert_eq!(report.totals.verified, 1);
        assert_eq!(report.totals.failed, 0);
        let object = &report.objects[0];
        assert_eq!(object.representation, representation);
        assert_eq!(object.map_status, "map_agrees");
        assert_eq!(object.verification_status, verification_status);
        assert_eq!(
            object.object_id,
            Value::String("00000000-0000-4000-8000-000000000001".to_string())
        );
    }

    #[test]
    fn parity_protected_plaintext_key_30_image_is_attested_and_green() {
        assert_attested_key_30_image(KEY_30_PLAINTEXT_IMAGE, "plaintext", "manifest_verified");
    }

    #[test]
    fn encrypted_key_30_image_is_attested_and_green() {
        assert_attested_key_30_image(KEY_30_ENCRYPTED_IMAGE, "encrypted", "envelope_consistent");
    }

    #[test]
    fn rows_outside_scope_are_not_failures_and_in_scope_map_mismatches_are() {
        let row = BootstrapObjectRow::plaintext(1, 2, 0, 1, 1, [0u8; 32]);
        let map = FilemarkMap::new(vec![TapeFileMapEntry::object(0, 1, 0)])
            .expect("one-object map is valid");
        let mut source = ImageDirectoryRawSource::from_tape_files(vec![vec![0u8; 4096]], 4096)
            .expect("one-object image wraps");

        let beyond = report_object_row(&mut source, &map, 1, 4096, &row);
        assert_eq!(beyond.map_status, "beyond_recovered_scope");
        assert_eq!(beyond.verification_status, "not_checked_beyond_scope");

        let mismatching_row = BootstrapObjectRow::plaintext(0, 2, 0, 1, 1, [0u8; 32]);
        let mismatch = report_object_row(&mut source, &map, 1, 4096, &mismatching_row);
        assert_eq!(mismatch.map_status, "map_mismatch");
        assert!(mismatch
            .map_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("stored block count mismatch")));
        assert_eq!(mismatch.verification_status, "not_checked");
    }

    #[test]
    fn unreadable_manifest_block_is_a_precise_row_failure_and_exit_two() {
        let temp = extract_archive_entries(&[OBJECT_ID_IMAGE]);
        let image = temp.path().join(OBJECT_ID_IMAGE);
        let mut source =
            ImageDirectoryRawSource::open(&image).expect("pinned plaintext image opens");
        source
            .mark_unreadable(1, 4)
            .expect("pinned manifest block exists");
        let candidates = source.candidate_block_sizes().to_vec();
        let report =
            build_recovery_report(&mut source, &candidates).expect("damage remains reportable");
        assert!(!report.success);
        assert_eq!(report.totals.failed, 1);
        assert_eq!(
            report.objects[0].verification_status,
            "manifest_unreadable_blocks"
        );
        assert!(report.objects[0]
            .verification_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("block 4")));

        let mut source =
            ImageDirectoryRawSource::open(&image).expect("pinned plaintext image reopens");
        source
            .mark_unreadable(1, 4)
            .expect("pinned manifest block exists");
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit =
            run_raw_recovery_report(&mut source, &candidates, true, &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::from(2));
        assert!(stderr.is_empty());
    }

    #[test]
    fn manifest_byte_damage_is_digest_mismatch_and_exit_two() {
        let temp = extract_archive_entries(&[OBJECT_ID_IMAGE]);
        let image = temp.path().join(OBJECT_ID_IMAGE);
        let object_path = image.join("tape-file-001-object.bin");
        let mut object = fs::read(&object_path).expect("pinned image object reads");
        object[4 * 4096] ^= 1;
        fs::write(&object_path, object)
            .expect("fault injection rewrites only the extracted temporary image");

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let exit = run_image_recovery_report(&image, true, &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::from(2));
        assert!(stderr.is_empty());
        let report: Value = serde_json::from_slice(&stdout).expect("JSON failure report parses");
        assert_eq!(
            report["objects"][0]["verification_status"],
            "manifest_digest_mismatch"
        );
        assert_eq!(report["totals"]["failed"], 1);
    }

    #[test]
    fn encrypted_publication_object_exercises_key_21_through_23_checks() {
        let temp = extract_archive_entries(&[ENCRYPTED_OBJECT, ENCRYPTED_MANIFEST]);
        let object =
            fs::read(temp.path().join(ENCRYPTED_OBJECT)).expect("pinned encrypted object reads");
        let manifest: Value = serde_json::from_slice(
            &fs::read(temp.path().join(ENCRYPTED_MANIFEST))
                .expect("pinned encrypted manifest reads"),
        )
        .expect("pinned encrypted manifest parses");
        let block_size = manifest["inputs"]["chunk_size"]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .expect("pinned chunk size fits u32");
        let stored_block_count = manifest["expected"]["stored_size_blocks"]
            .as_u64()
            .expect("pinned stored block count exists");
        let metadata_frame_len = manifest["expected"]["metadata_frame_len"]
            .as_u64()
            .expect("pinned metadata frame length exists");
        let key_frame_len = manifest["expected"]["key_frame_len"]
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .expect("pinned key frame length fits u32");
        let recipient_epoch_ids = manifest["inputs"]["recipients"]
            .as_array()
            .expect("pinned recipients are an array")
            .iter()
            .map(|recipient| {
                decode_hex_16(
                    recipient["recipient_epoch_id"]
                        .as_str()
                        .expect("pinned recipient epoch id is text"),
                )
            })
            .collect::<Vec<_>>();
        let row = BootstrapObjectRow::encrypted(
            0,
            stored_block_count,
            recipient_epoch_ids.clone(),
            metadata_frame_len,
            key_frame_len,
        )
        .with_object_id(
            manifest["inputs"]["object_id"]
                .as_str()
                .expect("pinned object id exists")
                .as_bytes()
                .to_vec(),
        );
        let map = FilemarkMap::new(vec![TapeFileMapEntry::object(0, stored_block_count, 0)])
            .expect("one-object map is valid");
        let mut source = ImageDirectoryRawSource::from_tape_files(vec![object.clone()], block_size)
            .expect("pinned encrypted object wraps as one tape file");
        source
            .configure_fixed_block_size(block_size)
            .expect("pinned encrypted image configures");
        let verification = verify_encrypted_envelope(
            &mut source,
            &map,
            &row,
            block_size,
            &recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        );
        assert_eq!(verification.status, "envelope_consistent");
        assert_eq!(verification.detail, None);

        let footer_offset = object
            .windows(REM_OBJECT_FOOTER.len())
            .rposition(|window| window == REM_OBJECT_FOOTER)
            .expect("pinned encrypted object carries completion footer");
        let mut damaged_object = object;
        damaged_object[footer_offset] ^= 1;
        let mut damaged_source =
            ImageDirectoryRawSource::from_tape_files(vec![damaged_object], block_size)
                .expect("damaged encrypted object wraps as one tape file");
        damaged_source
            .configure_fixed_block_size(block_size)
            .expect("damaged encrypted image configures");
        let damaged = verify_encrypted_envelope(
            &mut damaged_source,
            &map,
            &row,
            block_size,
            &recipient_epoch_ids,
            metadata_frame_len,
            key_frame_len,
        );
        assert_eq!(damaged.status, "envelope_completion_invalid");
    }

    #[test]
    fn non_utf8_object_identity_is_base64_in_json_and_lossy_for_humans() {
        let bytes = vec![0xff, b'A'];
        let (json_value, encoding, human) = render_object_id(&Some(bytes.clone()));
        assert_eq!(json_value, Value::String(BASE64_STANDARD.encode(bytes)));
        assert_eq!(encoding, "base64");
        assert!(human.contains('\u{fffd}'));
    }

    #[test]
    fn absent_legacy_object_identity_uses_the_required_marker() {
        let (json_value, encoding, human) = render_object_id(&None);
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

        let temp = extract_archive_entries(&[MINIMAL_IMAGE]);
        let mut source = ImageDirectoryRawSource::open(temp.path().join(MINIMAL_IMAGE))
            .expect("pinned minimal image opens");
        let candidates = source.candidate_block_sizes().to_vec();
        let mut stdout = FailingWriter;
        let mut stderr = Vec::new();
        let exit =
            run_raw_recovery_report(&mut source, &candidates, false, &mut stdout, &mut stderr);
        assert_eq!(exit, ExitCode::from(1));
        assert!(String::from_utf8(stderr)
            .expect("error is UTF-8")
            .contains("write human recovery report"));
    }

    fn decode_hex_16(value: &str) -> [u8; 16] {
        assert_eq!(value.len(), 32, "pinned epoch id is 16 bytes");
        let mut output = [0u8; 16];
        for (index, byte) in output.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16)
                .expect("pinned epoch id is lowercase hex");
        }
        output
    }
}
