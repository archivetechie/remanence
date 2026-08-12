//! Catalog-less filemark-map reconstruction for Layer 3c v0.4.4.
//!
//! The scanner walks physical tape files from BOT, reads only the first block
//! of each file for structural classification, and measures file length by
//! spacing to the next filemark. Bootstrap, parity-map, and sidecar tape files
//! are accepted only after their magic plus CRC/header validation succeeds.
//! Terminal replica/separation magic is structurally reserved: damaged terminal
//! framing remains typed control evidence so it cannot consume Object ordinals
//! or be rewritten by a conflict overlay.

use crate::bootstrap::{has_bootstrap_magic, parse_bootstrap_block, BootstrapPayload};
use crate::error::ParityError;
use crate::filemark_map::{
    FilemarkMap, FilemarkMapBuilder, FilemarkMapDigest, ScopedFilemarkMap, TapeFileKind,
    TapeFileMapEntry, TapeFilePosition,
};
use crate::index_separation::{
    derive_index_separation_footer_magic, derive_index_separation_header_magic,
    parse_index_separation_footer, parse_index_separation_header,
};
use crate::parity_map::{
    classify_parity_map_header_block, parse_parity_map_tape_file_with_unreadable_blocks,
    DecodedParityMapTapeFile, SidecarEpochDirectory,
};
use crate::raw::{
    tape_error_is_current_medium_damage, PhysicalPositionHint, RawReadOutcome, RawTapeSource,
};
use crate::sidecar::{
    classify_sidecar_header_block, parse_sidecar_footer_block, parse_sidecar_index_blocks,
    SidecarFooter, SidecarHeader,
};
use crate::tape_index_replica::{
    derive_tape_index_replica_footer_magic, derive_tape_index_replica_header_magic,
    parse_tape_index_bootstrap_footer, parse_tape_index_replica_header,
};
#[cfg(test)]
use remanence_library::TapeIoError;
use std::time::{Duration, Instant};

/// Catalog-supplied filemark map and protection watermark for a loaded tape.
///
/// Layer 5 should populate this from the same catalog tape row used to select
/// the loaded cartridge. The tape UUID is checked against the authoritative
/// bootstrap before the catalog map is trusted, catching catalog/tape swaps at
/// the Layer 3c API boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogFilemarkMapInput {
    /// Tape UUID recorded by the catalog for the loaded tape.
    pub tape_uuid: [u8; 16],
    /// Catalog projection of filemark-delimited tape files.
    pub map: FilemarkMap,
    /// Catalog's committed `highest_protected_ordinal` watermark.
    pub highest_protected_ordinal: u64,
}

/// Structural signature that terminated a physical tape walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanTailTruncationKind {
    /// End-of-data was reached before the current file's trailing filemark.
    MissingTrailingFilemark,
    /// Filemark spacing measured a file containing no data blocks.
    ZeroBlockFile,
    /// A filemark was encountered where the next file's first block belonged.
    EmptyFile,
}

/// First structurally incomplete tape file encountered by a physical walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanTailTruncation {
    /// Dense tape-file number the incomplete file would have occupied.
    pub tape_file_number: u64,
    /// Physical start position of the incomplete file.
    pub position: PhysicalPositionHint,
    /// Structural signature observed at that position.
    pub kind: ScanTailTruncationKind,
}

/// One structurally complete file beyond the digest-attested prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnattestedTapeFile {
    /// Forensic map entry. It is not eligible for recovery input.
    pub entry: TapeFileMapEntry,
    /// Physical start position measured by the walk.
    pub position: PhysicalPositionHint,
}

/// Tail-aware result of the single physical filemark-map walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanWalkResult {
    /// Structurally complete files walked before EOD or truncation.
    pub map: FilemarkMap,
    /// First incomplete tail file, when one terminated the walk.
    pub truncation: Option<ScanTailTruncation>,
    /// Best structural classification of the torn tail file from its readable
    /// first block. Recognisable terminal control magic is always preserved as
    /// control evidence and never falls through to Object.
    pub truncation_candidate_kind: Option<TapeFileKind>,
    /// Valid bootstrap copies encountered and structurally classified by the
    /// walk, in physical tape-file order.
    pub bootstrap_candidates: Vec<ScanBootstrapCandidate>,
    /// Physical damage encountered by the scanner itself.
    pub damaged_regions: Vec<ScanDamagedRegion>,
    unreadable_one_block_objects: Vec<u64>,
}

/// One bounded progress observation after a complete tape file was crossed.
///
/// The reported position is the scanner's best-known position immediately
/// after the completed file. A controller may stop the walk at this boundary;
/// the scanner will not read the next tape file after an abort decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanWalkProgress {
    /// Dense number of the tape file that was just crossed.
    pub tape_file_number: u64,
    /// Best-known physical position immediately after that tape file.
    pub position: PhysicalPositionHint,
    /// Structurally complete tape-file candidates accumulated so far.
    pub structural_candidate_count: u64,
    /// Time elapsed since the physical BOT walk began.
    pub elapsed: Duration,
}

/// Caller decision at a safe between-files scan boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanWalkControl {
    /// Continue with the next tape file.
    Continue,
    /// Stop before reading the next tape file.
    Abort,
}

/// Evidence retained when a controller stops a BOT walk between tape files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanWalkAbort {
    /// Last complete tape file crossed before the stop.
    pub last_tape_file_number: u64,
    /// Best-known physical position when the stop was honored.
    pub position: PhysicalPositionHint,
    /// Structurally complete tape-file candidates accumulated before the stop.
    pub structural_candidate_count: u64,
    /// Time elapsed since the physical BOT walk began.
    pub elapsed: Duration,
}

/// Terminal result of a controller-aware physical BOT walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlledScanWalkOutcome {
    /// The walk reached EOD or a typed tail truncation.
    Complete(ScanWalkResult),
    /// The controller stopped the walk at a safe between-files boundary.
    Aborted(ScanWalkAbort),
}

impl ScanWalkResult {
    /// Return the sole valid tape-file-0 BOT Bootstrap.
    pub fn authoritative_bootstrap(&self) -> Option<&ScanBootstrapCandidate> {
        self.bootstrap_candidates.first()
    }
}

/// One valid bootstrap copy encountered during the structural walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanBootstrapCandidate {
    /// Dense tape-file number containing the bootstrap.
    pub tape_file_number: u64,
    /// Fully parsed bootstrap payload.
    pub payload: BootstrapPayload,
}

/// Scanner-observed physical damage category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanDamageKind {
    /// The first block of a measured tape file was unreadable.
    UnreadableTapeFileHead,
    /// A tape file carried a recognisable structural header whose recorded
    /// block count disagreed with the measured length of the file, so the
    /// classification rung was abandoned and the file fell through to the next
    /// rung (REM-PARITY 12.3). The walk continues; the failure is reported.
    ClassificationCountMismatch,
    /// A terminal-control magic was present but its frame or measured count
    /// was invalid. It remains a control file and never consumes Object
    /// ordinals or participates in Object-based overlays.
    InvalidTerminalControl,
}

/// One contiguous damaged region encountered by the structural scanner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanDamagedRegion {
    /// First damaged physical position.
    pub start: PhysicalPositionHint,
    /// Number of consecutive blocks represented by this entry.
    pub block_count: u64,
    /// Scanner operation that encountered the damage.
    pub kind: ScanDamageKind,
}

/// Source that supplied structural-kind overlay information.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanOverlaySource {
    /// No overlay was required; the structural walk supplied the map.
    StructuralWalk,
    /// A catalog supplied the complete map.
    Catalog,
    /// Redundant structurally discovered parity-map files supplied it.
    StructurallySelectedParityMap,
}

/// Digest-validated scan result with an explicit attested/tail boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilemarkMapScanResult {
    /// Only the digest-attested map prefix reported as validated.
    pub attested_map: FilemarkMap,
    /// Recovery scope plus the full complete-file walk for guarded navigation.
    pub scoped_map: ScopedFilemarkMap,
    /// Structurally complete files beyond `attested_map`, for reporting only.
    pub unattested_files: Vec<UnattestedTapeFile>,
    /// First structurally incomplete tail file, when present.
    pub truncation: Option<ScanTailTruncation>,
    /// Equal-ranking parity-map copies whose validated payloads disagreed.
    pub parity_map_content_conflicts: Vec<ParityMapContentConflict>,
    /// Bootstrap sequence whose scope governed map validation.
    pub authoritative_bootstrap_sequence: u64,
    /// Source of any structural-kind overlay applied before digest validation.
    pub overlay_source: ScanOverlaySource,
    /// Physical damage encountered by the underlying structural scan.
    pub damaged_regions: Vec<ScanDamagedRegion>,
}

impl FilemarkMapScanResult {
    /// Number of structurally complete, unattested tail files.
    pub fn unattested_file_count(&self) -> usize {
        self.unattested_files.len()
    }
}

/// Ranking tuple used to select a structurally discovered parity map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParityMapSelectionKey {
    /// Whether the directory claims the complete structurally walked tape.
    pub is_final_directory: bool,
    /// Writer-assigned parity-map sequence.
    pub sequence: u64,
    /// Total object-data ordinals in the validated directory scope.
    pub directory_scope_total_data_ordinals: u64,
}

/// Non-fatal structural inconsistency between equal-ranking parity maps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityMapContentConflict {
    /// Tape-file numbers of the equal-ranking candidates, in ascending order.
    pub candidate_tape_file_numbers: Vec<u64>,
    /// Shared ranking tuple.
    pub selection_key: ParityMapSelectionKey,
    /// Lowest tape-file number selected as authoritative.
    pub chosen_tape_file_number: u64,
}

impl CatalogFilemarkMapInput {
    /// Construct a catalog map input for [`acquire_filemark_map`].
    pub fn new(tape_uuid: [u8; 16], map: FilemarkMap, highest_protected_ordinal: u64) -> Self {
        Self {
            tape_uuid,
            map,
            highest_protected_ordinal,
        }
    }
}

/// Acquire the authoritative Layer 3c filemark map for read/recovery setup.
///
/// If Layer 5 has a committed catalog map, that catalog path is authoritative
/// and no physical scan is performed. Otherwise this scans the tape and
/// validates the reconstructed map against the authoritative bootstrap's
/// `filemark_map_digest` or a higher-scope structurally selected parity-map
/// digest, preserving the selected authority's prefix scope.
pub fn acquire_filemark_map(
    source: &mut dyn RawTapeSource,
    authoritative_bootstrap: &BootstrapPayload,
    catalog_map: Option<CatalogFilemarkMapInput>,
) -> Result<ScopedFilemarkMap, ParityError> {
    Ok(acquire_filemark_map_with_report(source, authoritative_bootstrap, catalog_map)?.scoped_map)
}

/// Acquire a filemark map and retain the scanner's tail classification.
///
/// The legacy [`acquire_filemark_map`] wrapper returns only `scoped_map`.
/// Bare-tape reporting should use this surface so unattested complete files
/// and a torn final file cannot be mistaken for digest-attested rows.
pub fn acquire_filemark_map_with_report(
    source: &mut dyn RawTapeSource,
    authoritative_bootstrap: &BootstrapPayload,
    catalog_map: Option<CatalogFilemarkMapInput>,
) -> Result<FilemarkMapScanResult, ParityError> {
    if !authoritative_bootstrap.no_parity_flag && authoritative_bootstrap.drive_compression {
        return Err(ParityError::DriveCompressionEnabled);
    }

    if let Some(catalog) = catalog_map {
        validate_catalog_scope(&catalog, authoritative_bootstrap)?;
        let scoped_map =
            ScopedFilemarkMap::from_catalog(catalog.map, catalog.highest_protected_ordinal)
                .with_sidecar_directory(None);
        return filemark_map_scan_result(
            scoped_map,
            None,
            Vec::new(),
            authoritative_bootstrap.sequence,
            ScanOverlaySource::Catalog,
            Vec::new(),
        );
    }

    if authoritative_bootstrap.filemark_map_digest.is_none() {
        return Err(filemark_scan_error(
            "authoritative bootstrap does not carry a filemark-map digest",
        ));
    }
    let reconstructed = scan_reconstruct_filemark_map_with_report(
        source,
        &authoritative_bootstrap.tape_uuid,
        authoritative_bootstrap.block_size_bytes,
    )?;
    validate_scan_reconstruction_with_report(source, authoritative_bootstrap, reconstructed)
}

/// Validate one already-completed structural scan against a bootstrap scope.
///
/// Catalog-less report consumers call the structural scan once, select the
/// authoritative bootstrap from its candidates, and pass that same walk here.
/// This preserves the scanner's unreadable-head provenance for bootstrap
/// re-typing and applies the existing directory-overlay and digest funnel
/// without a second physical tape walk.
pub fn validate_scan_reconstruction_with_report(
    source: &mut dyn RawTapeSource,
    authoritative_bootstrap: &BootstrapPayload,
    reconstructed: ScanWalkResult,
) -> Result<FilemarkMapScanResult, ParityError> {
    let Some(digest) = authoritative_bootstrap.filemark_map_digest.as_ref() else {
        return Err(filemark_scan_error(
            "authoritative bootstrap does not carry a filemark-map digest",
        ));
    };
    match validate_scan_hypothesis(
        source,
        reconstructed.map.clone(),
        &reconstructed.unreadable_one_block_objects,
        authoritative_bootstrap,
        digest,
    ) {
        Ok(validated) => filemark_map_scan_result(
            validated.scoped_map,
            reconstructed.truncation,
            validated.parity_map_content_conflicts,
            authoritative_bootstrap.sequence,
            validated.overlay_source,
            reconstructed.damaged_regions,
        ),
        Err(original_error) => Err(enrich_scan_error_with_truncation(
            original_error,
            reconstructed.truncation,
        )),
    }
}

fn filemark_map_scan_result(
    scoped_map: ScopedFilemarkMap,
    truncation: Option<ScanTailTruncation>,
    parity_map_content_conflicts: Vec<ParityMapContentConflict>,
    authoritative_bootstrap_sequence: u64,
    overlay_source: ScanOverlaySource,
    damaged_regions: Vec<ScanDamagedRegion>,
) -> Result<FilemarkMapScanResult, ParityError> {
    let attested_tape_file_count = scoped_map
        .validated_prefix_tape_files
        .unwrap_or(scoped_map.map.tape_file_count());
    let attested_map = scoped_map
        .map
        .truncate_to_tape_files(attested_tape_file_count)?;
    let tail_start = usize::try_from(attested_tape_file_count)
        .map_err(|_| filemark_scan_error("attested tape-file count does not fit usize"))?;
    let mut unattested_files =
        Vec::with_capacity(scoped_map.map.entries().len().saturating_sub(tail_start));
    for entry in &scoped_map.map.entries()[tail_start..] {
        let position = scoped_map.map.physical_position(TapeFilePosition {
            tape_file_number: entry.tape_file_number,
            block_within_file: 0,
        })?;
        unattested_files.push(UnattestedTapeFile {
            entry: entry.clone(),
            position,
        });
    }
    Ok(FilemarkMapScanResult {
        attested_map,
        scoped_map,
        unattested_files,
        truncation,
        parity_map_content_conflicts,
        authoritative_bootstrap_sequence,
        overlay_source,
        damaged_regions,
    })
}

fn enrich_scan_error_with_truncation(
    error: ParityError,
    truncation: Option<ScanTailTruncation>,
) -> ParityError {
    let Some(truncation) = truncation else {
        return error;
    };
    match error {
        ParityError::FilemarkMapDigestMismatch { .. } => ParityError::FilemarkMapDigestMismatch {
            truncation_position: Some(truncation.position),
        },
        ParityError::FilemarkMapReconstruct(message) => {
            ParityError::FilemarkMapReconstruct(format!(
                "{message}; walk terminated at tape file {} physical LBA {} ({:?})",
                truncation.tape_file_number, truncation.position.lba, truncation.kind
            ))
        }
        other => other,
    }
}

struct ValidatedScanHypothesis {
    scoped_map: ScopedFilemarkMap,
    parity_map_content_conflicts: Vec<ParityMapContentConflict>,
    overlay_source: ScanOverlaySource,
}

fn validate_scan_hypothesis(
    source: &mut dyn RawTapeSource,
    reconstructed: FilemarkMap,
    unreadable_one_block_objects: &[u64],
    authoritative_bootstrap: &BootstrapPayload,
    digest: &FilemarkMapDigest,
) -> Result<ValidatedScanHypothesis, ParityError> {
    let overlay = apply_authoritative_directory_overlay(
        source,
        reconstructed,
        unreadable_one_block_objects,
        authoritative_bootstrap,
    )?;
    let fencing_digest = overlay.fencing_digest.as_ref().unwrap_or(digest);
    let scoped_map = ScopedFilemarkMap::validate_against_digest(overlay.map, fencing_digest)?
        .with_sidecar_directory(overlay.sidecar_directory);
    Ok(ValidatedScanHypothesis {
        scoped_map,
        parity_map_content_conflicts: overlay.parity_map_content_conflicts,
        overlay_source: overlay.source,
    })
}

/// Reconstruct a structural filemark map by scanning the tape file by file.
///
/// `tape_uuid` comes from a valid bootstrap discovered before this scan; it is
/// required to derive the HMAC sidecar magic. The caller is expected to compare
/// the returned map with the authoritative bootstrap digest via
/// [`crate::ScopedFilemarkMap::validate_against_digest`]. If scanning completes
/// but that digest check fails, one possible cause is that the caller used a
/// block size from the wrong bootstrap or tape, not only physical corruption.
pub fn scan_reconstruct_filemark_map(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<FilemarkMap, ParityError> {
    Ok(scan_reconstruct_filemark_map_with_report(source, tape_uuid, block_size)?.map)
}

/// Walk structurally complete tape files and report the first torn tail file.
///
/// This is the reporting form of [`scan_reconstruct_filemark_map`]; both use
/// the same walk and classification funnel.
pub fn scan_reconstruct_filemark_map_with_report(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<ScanWalkResult, ParityError> {
    let outcome =
        scan_reconstruct_filemark_map_with_control(source, tape_uuid, block_size, |_| {
            ScanWalkControl::Continue
        })?;
    let ControlledScanWalkOutcome::Complete(walked) = outcome else {
        unreachable!("an unconditional scan controller cannot abort")
    };
    Ok(walked)
}

/// Walk from BOT with bounded progress and a between-files stop decision.
///
/// The callback runs exactly once after each structurally complete tape file.
/// Returning [`ScanWalkControl::Abort`] stops before the next tape file is read.
pub fn scan_reconstruct_filemark_map_with_control<F>(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    mut control: F,
) -> Result<ControlledScanWalkOutcome, ParityError>
where
    F: FnMut(&ScanWalkProgress) -> ScanWalkControl,
{
    match scan_reconstruct_filemark_map_with_provenance(
        source,
        tape_uuid,
        block_size,
        &mut control,
    )? {
        ScanReconstructionOutcome::Complete(reconstructed) => {
            Ok(ControlledScanWalkOutcome::Complete(ScanWalkResult {
                map: reconstructed.map,
                truncation: reconstructed.truncation,
                truncation_candidate_kind: reconstructed.truncation_candidate_kind,
                bootstrap_candidates: reconstructed.bootstrap_candidates,
                damaged_regions: reconstructed.damaged_regions,
                unreadable_one_block_objects: reconstructed.unreadable_one_block_objects,
            }))
        }
        ScanReconstructionOutcome::Aborted(aborted) => {
            Ok(ControlledScanWalkOutcome::Aborted(aborted))
        }
    }
}

#[derive(Debug)]
struct ScanReconstruction {
    map: FilemarkMap,
    unreadable_one_block_objects: Vec<u64>,
    truncation: Option<ScanTailTruncation>,
    truncation_candidate_kind: Option<TapeFileKind>,
    bootstrap_candidates: Vec<ScanBootstrapCandidate>,
    damaged_regions: Vec<ScanDamagedRegion>,
}

enum ScanReconstructionOutcome {
    Complete(ScanReconstruction),
    Aborted(ScanWalkAbort),
}

fn scan_reconstruct_filemark_map_with_provenance<F>(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    control: &mut F,
) -> Result<ScanReconstructionOutcome, ParityError>
where
    F: FnMut(&ScanWalkProgress) -> ScanWalkControl,
{
    if block_size == 0 {
        return Err(ParityError::Invariant("scan block size is zero"));
    }

    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| ParityError::Invariant("scan block size does not fit usize"))?;
    source.configure_fixed_block_size(block_size)?;
    source.locate_physical(PhysicalPositionHint::new(0))?;

    let mut builder = FilemarkMapBuilder::new();
    let mut buf = vec![0u8; block_size_usize];
    let mut saw_file = false;
    let mut unreadable_one_block_objects = Vec::new();
    let mut truncation = None;
    let mut truncation_candidate_kind = None;
    let mut bootstrap_candidates = Vec::new();
    let mut damaged_regions = Vec::new();
    let started_at = Instant::now();

    loop {
        let file_start = source.position()?;
        match source.read_record(&mut buf) {
            Ok(RawReadOutcome::EndOfData { .. }) => break,
            Ok(RawReadOutcome::Filemark { .. }) => {
                truncation = Some(ScanTailTruncation {
                    tape_file_number: builder.next_tape_file_number()?,
                    position: file_start,
                    kind: ScanTailTruncationKind::EmptyFile,
                });
                break;
            }
            Ok(RawReadOutcome::Block { bytes, .. }) if bytes != block_size_usize => {
                return Err(filemark_scan_error(format!(
                    "short fixed-block scan read at physical LBA {}: got {bytes}, expected {block_size_usize}",
                    file_start.lba
                )));
            }
            Ok(RawReadOutcome::Block { .. }) => {
                let first_block = buf.clone();
                let measured = match measure_current_file(source, file_start)? {
                    MeasureCurrentFileOutcome::Complete(measured) => measured,
                    MeasureCurrentFileOutcome::Truncated(kind) => {
                        truncation_candidate_kind = Some(classify_truncated_file_head(
                            &first_block,
                            tape_uuid,
                            block_size,
                            builder.next_tape_file_number()? == 0 && file_start.lba == 0,
                        ));
                        truncation = Some(ScanTailTruncation {
                            tape_file_number: builder.next_tape_file_number()?,
                            position: file_start,
                            kind,
                        });
                        break;
                    }
                };
                if let Some(candidate) = append_classified_entry(
                    source,
                    &mut builder,
                    &first_block,
                    tape_uuid,
                    block_size,
                    file_start,
                    measured.block_count,
                    &mut damaged_regions,
                )? {
                    bootstrap_candidates.push(candidate);
                }
                source.locate_physical(measured.position_after)?;
                saw_file = true;
                if let Some(aborted) =
                    scan_boundary_control(&builder, measured.position_after, started_at, control)?
                {
                    return Ok(ScanReconstructionOutcome::Aborted(aborted));
                }
            }
            Err(error) if scan_read_error_is_medium_damage(&error) => {
                damaged_regions.push(ScanDamagedRegion {
                    start: file_start,
                    block_count: 1,
                    kind: ScanDamageKind::UnreadableTapeFileHead,
                });
                source.locate_physical(file_start)?;
                let measured = match measure_current_file(source, file_start)? {
                    MeasureCurrentFileOutcome::Complete(measured) => measured,
                    MeasureCurrentFileOutcome::Truncated(kind) => {
                        truncation = Some(ScanTailTruncation {
                            tape_file_number: builder.next_tape_file_number()?,
                            position: file_start,
                            kind,
                        });
                        break;
                    }
                };
                let tape_file_number = builder.next_tape_file_number()?;
                let classified_as_object = append_entry_with_unreadable_head(
                    source,
                    &mut builder,
                    tape_uuid,
                    block_size,
                    file_start,
                    measured.block_count,
                    &mut damaged_regions,
                )?;
                if measured.block_count == 1 && classified_as_object {
                    unreadable_one_block_objects.push(tape_file_number);
                }
                source.locate_physical(measured.position_after)?;
                saw_file = true;
                if let Some(aborted) =
                    scan_boundary_control(&builder, measured.position_after, started_at, control)?
                {
                    return Ok(ScanReconstructionOutcome::Aborted(aborted));
                }
            }
            Err(error) => return Err(error),
        }
    }

    if !saw_file && truncation.is_none() {
        return Err(filemark_scan_error("scan found no tape files"));
    }

    Ok(ScanReconstructionOutcome::Complete(ScanReconstruction {
        map: builder.build()?,
        unreadable_one_block_objects,
        truncation,
        truncation_candidate_kind,
        bootstrap_candidates,
        damaged_regions,
    }))
}

fn scan_boundary_control<F>(
    builder: &FilemarkMapBuilder,
    position: PhysicalPositionHint,
    started_at: Instant,
    control: &mut F,
) -> Result<Option<ScanWalkAbort>, ParityError>
where
    F: FnMut(&ScanWalkProgress) -> ScanWalkControl,
{
    let structural_candidate_count = builder.next_tape_file_number()?;
    let progress = ScanWalkProgress {
        tape_file_number: structural_candidate_count
            .checked_sub(1)
            .ok_or(ParityError::Invariant("completed scan file count is zero"))?,
        position,
        structural_candidate_count,
        elapsed: started_at.elapsed(),
    };
    Ok(
        (control(&progress) == ScanWalkControl::Abort).then_some(ScanWalkAbort {
            last_tape_file_number: progress.tape_file_number,
            position: progress.position,
            structural_candidate_count: progress.structural_candidate_count,
            elapsed: progress.elapsed,
        }),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MeasuredTapeFile {
    block_count: u64,
    position_after: PhysicalPositionHint,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MeasureCurrentFileOutcome {
    Complete(MeasuredTapeFile),
    Truncated(ScanTailTruncationKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SidecarScanClassification {
    epoch_id: u64,
    protected_ordinal_start: u64,
    protected_ordinal_end_exclusive: u64,
}

impl From<&SidecarHeader> for SidecarScanClassification {
    fn from(header: &SidecarHeader) -> Self {
        Self {
            epoch_id: header.epoch_id,
            protected_ordinal_start: header.protected_ordinal_start,
            protected_ordinal_end_exclusive: header.protected_ordinal_end_exclusive,
        }
    }
}

impl From<&SidecarFooter> for SidecarScanClassification {
    fn from(footer: &SidecarFooter) -> Self {
        Self {
            epoch_id: footer.epoch_id,
            protected_ordinal_start: footer.protected_ordinal_start,
            protected_ordinal_end_exclusive: footer.protected_ordinal_end_exclusive,
        }
    }
}

fn measure_current_file(
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
) -> Result<MeasureCurrentFileOutcome, ParityError> {
    let outcome = source.space_filemarks(1)?;
    if outcome.filemarks_spaced != 1 {
        return Ok(MeasureCurrentFileOutcome::Truncated(
            ScanTailTruncationKind::MissingTrailingFilemark,
        ));
    }

    let consumed = outcome
        .position_after
        .lba
        .checked_sub(file_start.lba)
        .ok_or_else(|| filemark_scan_error("scan position moved before file start"))?;
    let block_count = consumed
        .checked_sub(1)
        .ok_or_else(|| filemark_scan_error("scan filemark position underflow"))?;
    if block_count == 0 {
        return Ok(MeasureCurrentFileOutcome::Truncated(
            ScanTailTruncationKind::ZeroBlockFile,
        ));
    }
    Ok(MeasureCurrentFileOutcome::Complete(MeasuredTapeFile {
        block_count,
        position_after: outcome.position_after,
    }))
}

fn classify_truncated_file_head(
    block0: &[u8],
    tape_uuid: &[u8; 16],
    block_size: u32,
    is_bot_file: bool,
) -> TapeFileKind {
    let has_magic = |magic: [u8; 8]| block0.get(..8).is_some_and(|prefix| prefix == magic);
    if has_magic(derive_tape_index_replica_header_magic(tape_uuid)) {
        return TapeFileKind::TapeIndexReplica;
    }
    if has_magic(derive_index_separation_header_magic(tape_uuid)) {
        return TapeFileKind::IndexSeparationExtent;
    }
    if is_bot_file
        && has_bootstrap_magic(block0)
        && parse_bootstrap_block(block0).is_ok_and(|payload| {
            payload.block_size_bytes == block_size && payload.tape_uuid == *tape_uuid
        })
    {
        return TapeFileKind::Bootstrap;
    }
    if classify_parity_map_header_block(block0, tape_uuid).is_ok_and(|header| header.is_some()) {
        return TapeFileKind::ParityMap;
    }
    if classify_sidecar_header_block(block0, tape_uuid).is_ok_and(|header| header.is_some()) {
        return TapeFileKind::ParitySidecar;
    }
    TapeFileKind::Object
}

#[allow(clippy::too_many_arguments)]
fn append_classified_entry(
    source: &mut dyn RawTapeSource,
    builder: &mut FilemarkMapBuilder,
    block0: &[u8],
    tape_uuid: &[u8; 16],
    block_size: u32,
    file_start: PhysicalPositionHint,
    block_count: u64,
    damaged_regions: &mut Vec<ScanDamagedRegion>,
) -> Result<Option<ScanBootstrapCandidate>, ParityError> {
    // REM-PARITY 12.3: a count mismatch at a classification rung abandons that
    // rung for this tape file only. It MUST NOT abort the walk — the catalog-less
    // reader needs the rest of the map, and the bootstrap re-typing and
    // parity_map overlay rescues (12.4) run only after the walk completes.
    let note_count_mismatch = |damaged_regions: &mut Vec<ScanDamagedRegion>| {
        damaged_regions.push(ScanDamagedRegion {
            start: file_start,
            block_count,
            kind: ScanDamageKind::ClassificationCountMismatch,
        });
    };
    let has_magic = |magic: [u8; 8]| block0.get(..8).is_some_and(|prefix| prefix == magic);
    if has_magic(derive_tape_index_replica_header_magic(tape_uuid)) {
        let valid_count = parse_tape_index_replica_header(block0, tape_uuid)
            .is_ok_and(|header| header.plan.component.record_count == block_count);
        if !valid_count {
            damaged_regions.push(ScanDamagedRegion {
                start: file_start,
                block_count,
                kind: ScanDamageKind::InvalidTerminalControl,
            });
        }
        builder.push_tape_index_replica(block_count)?;
        return Ok(None);
    }
    if has_magic(derive_index_separation_header_magic(tape_uuid)) {
        let valid_count = parse_index_separation_header(block0, tape_uuid)
            .is_ok_and(|header| header.plan.component.record_count == block_count);
        if !valid_count {
            damaged_regions.push(ScanDamagedRegion {
                start: file_start,
                block_count,
                kind: ScanDamageKind::InvalidTerminalControl,
            });
        }
        builder.push_index_separation_extent(block_count)?;
        return Ok(None);
    }
    if builder.next_tape_file_number()? == 0 && file_start.lba == 0 && has_bootstrap_magic(block0) {
        match parse_bootstrap_block(block0) {
            Ok(payload) => {
                if payload.block_size_bytes == block_size && payload.tape_uuid == *tape_uuid {
                    if block_count != 1 {
                        note_count_mismatch(damaged_regions);
                        builder.push_object(block_count)?;
                        return Ok(None);
                    }
                    let tape_file_number = builder.next_tape_file_number()?;
                    builder.push_bootstrap()?;
                    return Ok(Some(ScanBootstrapCandidate {
                        tape_file_number,
                        payload,
                    }));
                }
            }
            Err(ParityError::DriveCompressionEnabled) => {
                return Err(ParityError::DriveCompressionEnabled);
            }
            Err(_) => {}
        }
    }

    if let Ok(Some(header)) = classify_parity_map_header_block(block0, tape_uuid) {
        let expected = header.parity_map_total_block_count;
        if block_count != expected {
            note_count_mismatch(damaged_regions);
            builder.push_object(block_count)?;
            return Ok(None);
        }
        builder.push_parity_map(block_count)?;
        return Ok(None);
    }

    if let Ok(Some(header)) = classify_sidecar_header_block(block0, tape_uuid) {
        let expected = header.sidecar_total_block_count;
        if block_count != expected {
            note_count_mismatch(damaged_regions);
            builder.push_object(block_count)?;
            return Ok(None);
        }
        builder.push_parity_sidecar(
            block_count,
            header.epoch_id,
            header.protected_ordinal_start,
            header.protected_ordinal_end_exclusive,
        )?;
        return Ok(None);
    }

    if let Some(kind) = classify_terminal_from_footer_tail(
        source,
        file_start,
        tape_uuid,
        block_size,
        block_count,
        damaged_regions,
    )? {
        match kind {
            TerminalControlScanClassification::Replica => {
                builder.push_tape_index_replica(block_count)?;
            }
            TerminalControlScanClassification::Separation => {
                builder.push_index_separation_extent(block_count)?;
            }
        }
        return Ok(None);
    }

    if let Some(header) = classify_sidecar_from_footer_tail(
        source,
        file_start,
        tape_uuid,
        block_size,
        block_count,
        damaged_regions,
    )? {
        builder.push_parity_sidecar(
            block_count,
            header.epoch_id,
            header.protected_ordinal_start,
            header.protected_ordinal_end_exclusive,
        )?;
        return Ok(None);
    }

    builder.push_object(block_count)?;
    Ok(None)
}

fn append_entry_with_unreadable_head(
    source: &mut dyn RawTapeSource,
    builder: &mut FilemarkMapBuilder,
    tape_uuid: &[u8; 16],
    block_size: u32,
    file_start: PhysicalPositionHint,
    block_count: u64,
    damaged_regions: &mut Vec<ScanDamagedRegion>,
) -> Result<bool, ParityError> {
    if builder.next_tape_file_number()? == 0 && file_start.lba == 0 {
        if block_count != 1 {
            return Err(filemark_scan_error(format!(
                "unreadable BOT tape file has {block_count} blocks; schema-major 2 requires one Bootstrap block"
            )));
        }
        // The caller supplied the expected tape UUID and fixed block size.
        // Those inputs safely classify the unreadable one-block physical BOT
        // as the required structural Bootstrap; they do not authenticate it.
        builder.push_bootstrap()?;
        return Ok(false);
    }
    if let Some(kind) = classify_terminal_from_footer_tail(
        source,
        file_start,
        tape_uuid,
        block_size,
        block_count,
        damaged_regions,
    )? {
        match kind {
            TerminalControlScanClassification::Replica => {
                builder.push_tape_index_replica(block_count)?;
            }
            TerminalControlScanClassification::Separation => {
                builder.push_index_separation_extent(block_count)?;
            }
        }
        return Ok(false);
    }
    if let Some(header) = classify_sidecar_from_footer_tail(
        source,
        file_start,
        tape_uuid,
        block_size,
        block_count,
        damaged_regions,
    )? {
        builder.push_parity_sidecar(
            block_count,
            header.epoch_id,
            header.protected_ordinal_start,
            header.protected_ordinal_end_exclusive,
        )?;
        Ok(false)
    } else {
        builder.push_object(block_count)?;
        Ok(true)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalControlScanClassification {
    Replica,
    Separation,
}

fn classify_terminal_from_footer_tail(
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
    tape_uuid: &[u8; 16],
    block_size: u32,
    block_count: u64,
    damaged_regions: &mut Vec<ScanDamagedRegion>,
) -> Result<Option<TerminalControlScanClassification>, ParityError> {
    let Some(footer_block) =
        read_optional_fixed_block_at(source, file_start, block_count - 1, block_size)?
    else {
        return Ok(None);
    };
    let magic = footer_block.get(..8);
    let replica_magic = derive_tape_index_replica_footer_magic(tape_uuid);
    if magic.is_some_and(|prefix| prefix == replica_magic) {
        let valid_count = parse_tape_index_bootstrap_footer(&footer_block, tape_uuid)
            .is_ok_and(|footer| footer.plan.component.record_count == block_count);
        if !valid_count {
            damaged_regions.push(ScanDamagedRegion {
                start: file_start,
                block_count,
                kind: ScanDamageKind::InvalidTerminalControl,
            });
        }
        return Ok(Some(TerminalControlScanClassification::Replica));
    }
    let separation_magic = derive_index_separation_footer_magic(tape_uuid);
    if magic.is_some_and(|prefix| prefix == separation_magic) {
        let valid_count = parse_index_separation_footer(&footer_block, tape_uuid)
            .is_ok_and(|footer| footer.plan.component.record_count == block_count);
        if !valid_count {
            damaged_regions.push(ScanDamagedRegion {
                start: file_start,
                block_count,
                kind: ScanDamageKind::InvalidTerminalControl,
            });
        }
        return Ok(Some(TerminalControlScanClassification::Separation));
    }
    Ok(None)
}

fn classify_sidecar_from_footer_tail(
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
    tape_uuid: &[u8; 16],
    block_size: u32,
    block_count: u64,
    damaged_regions: &mut Vec<ScanDamagedRegion>,
) -> Result<Option<SidecarScanClassification>, ParityError> {
    let Some(footer_block) =
        read_optional_fixed_block_at(source, file_start, block_count - 1, block_size)?
    else {
        return Ok(None);
    };
    let footer = match parse_sidecar_footer_block(&footer_block, tape_uuid) {
        Ok(footer) => footer,
        Err(_) => return Ok(None),
    };
    if footer.sidecar_total_block_count != block_count {
        // REM-PARITY 12.3: report and fall through to the next rung, rather than
        // abandoning the whole walk over one tape file's disagreement.
        damaged_regions.push(ScanDamagedRegion {
            start: file_start,
            block_count,
            kind: ScanDamageKind::ClassificationCountMismatch,
        });
        return Ok(None);
    }

    match read_tail_sidecar_header(source, file_start, tape_uuid, block_size, &footer)? {
        Some(header) => Ok(Some(SidecarScanClassification::from(&header))),
        None => Ok(Some(SidecarScanClassification::from(&footer))),
    }
}

fn read_tail_sidecar_header(
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
    tape_uuid: &[u8; 16],
    block_size: u32,
    footer: &SidecarFooter,
) -> Result<Option<SidecarHeader>, ParityError> {
    let mut blocks = Vec::with_capacity(
        usize::try_from(footer.sidecar_header_block_count)
            .ok()
            .unwrap_or(0),
    );
    for offset in 0..footer.sidecar_header_block_count {
        let Some(block) = read_optional_fixed_block_at(
            source,
            file_start,
            footer
                .tail_header_start_block
                .checked_add(offset)
                .ok_or_else(|| filemark_scan_error("sidecar tail header offset overflows"))?,
            block_size,
        )?
        else {
            return Ok(None);
        };
        blocks.push(block);
    }
    let decoded = match parse_sidecar_index_blocks(&blocks, tape_uuid) {
        Ok(decoded) => decoded,
        Err(_) => return Ok(None),
    };
    if !sidecar_header_matches_footer(&decoded.header, footer) {
        return Err(filemark_scan_error(format!(
            "sidecar tail header for epoch {} does not match footer locator",
            footer.epoch_id
        )));
    }
    Ok(Some(decoded.header))
}

fn read_optional_fixed_block_at(
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
    block_within_file: u64,
    block_size: u32,
) -> Result<Option<Vec<u8>>, ParityError> {
    let lba = file_start
        .lba
        .checked_add(block_within_file)
        .ok_or_else(|| filemark_scan_error("scan sidecar probe LBA overflows"))?;
    source.locate_physical(PhysicalPositionHint {
        lba,
        partition: file_start.partition,
    })?;
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| ParityError::Invariant("scan block size does not fit usize"))?;
    let mut buf = vec![0u8; block_size_usize];
    match source.read_record(&mut buf) {
        Ok(RawReadOutcome::Block { bytes, .. }) if bytes == block_size_usize => Ok(Some(buf)),
        Ok(RawReadOutcome::Block { .. })
        | Ok(RawReadOutcome::Filemark { .. })
        | Ok(RawReadOutcome::EndOfData { .. }) => Ok(None),
        Err(error) if scan_read_error_is_medium_damage(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn scan_read_error_is_medium_damage(error: &ParityError) -> bool {
    matches!(error, ParityError::TapeIo(error) if tape_error_is_current_medium_damage(error))
}

fn sidecar_header_matches_footer(header: &SidecarHeader, footer: &SidecarFooter) -> bool {
    header.tape_uuid == footer.tape_uuid
        && header.epoch_id == footer.epoch_id
        && header.protected_ordinal_start == footer.protected_ordinal_start
        && header.protected_ordinal_end_exclusive == footer.protected_ordinal_end_exclusive
        && header.shard_index_block_count == footer.sidecar_header_block_count
        && header.parity_block_count == footer.parity_shard_block_count
        && header.sidecar_total_block_count == footer.sidecar_total_block_count
        && header.primary_header_start_block == footer.primary_header_start_block
        && header.tail_header_start_block == footer.tail_header_start_block
        && header.canonical_metadata_hash == footer.canonical_metadata_hash
}

struct AuthoritativeDirectoryOverlay {
    map: FilemarkMap,
    fencing_digest: Option<FilemarkMapDigest>,
    parity_map_content_conflicts: Vec<ParityMapContentConflict>,
    source: ScanOverlaySource,
    /// The directory this overlay was built from, when there was one. Carried
    /// so the recovery path can run the REM-PARITY 13.3 step 3 tail rescue.
    sidecar_directory: Option<SidecarEpochDirectory>,
}

struct ValidatedParityMapCandidate {
    tape_file_number: u64,
    decoded: DecodedParityMapTapeFile,
    overlayed_map: FilemarkMap,
}

impl ValidatedParityMapCandidate {
    fn selection_key(&self) -> ParityMapSelectionKey {
        ParityMapSelectionKey {
            is_final_directory: self.decoded.payload.directory.is_final_directory,
            sequence: self.decoded.payload.sequence,
            directory_scope_total_data_ordinals: self
                .decoded
                .payload
                .directory
                .directory_scope_total_data_ordinals,
        }
    }

    fn fencing_digest(&self) -> FilemarkMapDigest {
        let directory = &self.decoded.payload.directory;
        FilemarkMapDigest {
            map_sha256: self.decoded.payload.canonical_map_digest,
            tape_file_count: directory.directory_scope_tape_file_count,
            map_total_data_ordinals: directory.directory_scope_total_data_ordinals,
            highest_protected_ordinal: directory.directory_scope_highest_protected_ordinal,
            covers_complete_map: directory.is_final_directory,
        }
    }
}

fn apply_authoritative_directory_overlay(
    source: &mut dyn RawTapeSource,
    reconstructed: FilemarkMap,
    unreadable_one_block_objects: &[u64],
    authoritative_bootstrap: &BootstrapPayload,
) -> Result<AuthoritativeDirectoryOverlay, ParityError> {
    if let Some(selected) = select_structurally_discovered_parity_map(
        source,
        &reconstructed,
        unreadable_one_block_objects,
        &authoritative_bootstrap.tape_uuid,
        authoritative_bootstrap.block_size_bytes,
    )? {
        return Ok(selected);
    }

    Ok(AuthoritativeDirectoryOverlay {
        map: reconstructed,
        fencing_digest: None,
        parity_map_content_conflicts: Vec::new(),
        source: ScanOverlaySource::StructuralWalk,
        sidecar_directory: None,
    })
}

fn select_structurally_discovered_parity_map(
    source: &mut dyn RawTapeSource,
    reconstructed: &FilemarkMap,
    unreadable_one_block_objects: &[u64],
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<Option<AuthoritativeDirectoryOverlay>, ParityError> {
    let structural_candidates: Vec<_> = reconstructed
        .entries()
        .iter()
        .filter(|entry| entry.kind == TapeFileKind::ParityMap)
        .collect();
    if structural_candidates.len() < 2 {
        return Ok(None);
    }

    let mut validated_candidates = Vec::new();
    for entry in structural_candidates {
        let Some(decoded) = read_structurally_discovered_parity_map(
            source,
            reconstructed,
            entry,
            tape_uuid,
            block_size,
        )?
        else {
            continue;
        };
        let Some(overlayed_map) = cross_check_structurally_discovered_parity_map(
            reconstructed,
            unreadable_one_block_objects,
            &decoded,
        )?
        else {
            continue;
        };
        validated_candidates.push(ValidatedParityMapCandidate {
            tape_file_number: entry.tape_file_number,
            decoded,
            overlayed_map,
        });
    }
    if validated_candidates.is_empty() {
        return Ok(None);
    }

    let greatest_key = validated_candidates
        .iter()
        .map(ValidatedParityMapCandidate::selection_key)
        .max_by_key(|key| {
            (
                key.is_final_directory,
                key.sequence,
                key.directory_scope_total_data_ordinals,
            )
        })
        .expect("non-empty candidate list has a greatest key");
    let mut tied_indices: Vec<_> = validated_candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            (candidate.selection_key() == greatest_key).then_some(index)
        })
        .collect();
    tied_indices.sort_by_key(|index| validated_candidates[*index].tape_file_number);
    let chosen_index = tied_indices[0];
    let chosen_payload = &validated_candidates[chosen_index].decoded.payload_bytes;
    let content_disagrees = tied_indices
        .iter()
        .any(|index| validated_candidates[*index].decoded.payload_bytes != *chosen_payload);
    let parity_map_content_conflicts = if content_disagrees {
        vec![ParityMapContentConflict {
            candidate_tape_file_numbers: tied_indices
                .iter()
                .map(|index| validated_candidates[*index].tape_file_number)
                .collect(),
            selection_key: greatest_key,
            chosen_tape_file_number: validated_candidates[chosen_index].tape_file_number,
        }]
    } else {
        Vec::new()
    };

    let selected = validated_candidates.swap_remove(chosen_index);
    Ok(Some(AuthoritativeDirectoryOverlay {
        fencing_digest: Some(selected.fencing_digest()),
        sidecar_directory: Some(selected.decoded.payload.directory.clone()),
        map: selected.overlayed_map,
        parity_map_content_conflicts,
        source: ScanOverlaySource::StructurallySelectedParityMap,
    }))
}

fn read_structurally_discovered_parity_map(
    source: &mut dyn RawTapeSource,
    reconstructed: &FilemarkMap,
    entry: &TapeFileMapEntry,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<Option<DecodedParityMapTapeFile>, ParityError> {
    let block_capacity = usize::try_from(entry.block_count).map_err(|_| {
        filemark_scan_error(format!(
            "structural parity_map {} block_count {} does not fit usize",
            entry.tape_file_number, entry.block_count
        ))
    })?;
    let file_start = reconstructed.physical_position(TapeFilePosition {
        tape_file_number: entry.tape_file_number,
        block_within_file: 0,
    })?;
    let mut blocks = Vec::with_capacity(block_capacity);
    for block_within_file in 0..entry.block_count {
        blocks.push(read_optional_fixed_block_at(
            source,
            file_start,
            block_within_file,
            block_size,
        )?);
    }
    match parse_parity_map_tape_file_with_unreadable_blocks(&blocks, tape_uuid) {
        Ok(decoded) => Ok(Some(decoded)),
        Err(_) => Ok(None),
    }
}

fn cross_check_structurally_discovered_parity_map(
    reconstructed: &FilemarkMap,
    _unreadable_one_block_objects: &[u64],
    decoded: &DecodedParityMapTapeFile,
) -> Result<Option<FilemarkMap>, ParityError> {
    let directory = &decoded.payload.directory;
    let structurally_complete_file_count = reconstructed.tape_file_count();
    if directory.directory_scope_tape_file_count > structurally_complete_file_count
        || (directory.is_final_directory
            && directory.directory_scope_tape_file_count != structurally_complete_file_count)
    {
        return Ok(None);
    }

    let overlayed = apply_sidecar_directory_overlay_projection(reconstructed.clone(), directory)?;
    let scoped = overlayed.truncate_to_tape_files(directory.directory_scope_tape_file_count)?;
    if scoped.canonical_digest()? != decoded.payload.canonical_map_digest
        || scoped.tape_file_count() != directory.directory_scope_tape_file_count
        || scoped.total_data_ordinals() != directory.directory_scope_total_data_ordinals
        || scoped.max_sidecar_end_exclusive() != directory.directory_scope_highest_protected_ordinal
    {
        return Ok(None);
    }
    Ok(Some(overlayed))
}

fn apply_sidecar_directory_overlay_projection(
    reconstructed: FilemarkMap,
    directory: &SidecarEpochDirectory,
) -> Result<FilemarkMap, ParityError> {
    directory.validate()?;
    let scope_len = usize::try_from(directory.directory_scope_tape_file_count).map_err(|_| {
        filemark_scan_error("sidecar directory scope tape-file count does not fit usize")
    })?;
    if scope_len > reconstructed.entries().len() {
        return Err(filemark_scan_error(format!(
            "sidecar directory scope {} exceeds scanned map length {}",
            directory.directory_scope_tape_file_count,
            reconstructed.entries().len()
        )));
    }

    let mut next_object_ordinal = 0u64;
    let mut overlayed_entries = Vec::with_capacity(reconstructed.entries().len());
    for entry in reconstructed.entries() {
        if let Some(directory_entry) = directory
            .entries
            .iter()
            .find(|directory_entry| directory_entry.tape_file_number == entry.tape_file_number)
        {
            let directory_entry_index =
                usize::try_from(directory_entry.tape_file_number).map_err(|_| {
                    filemark_scan_error(format!(
                        "sidecar directory entry {} does not fit usize",
                        directory_entry.tape_file_number
                    ))
                })?;
            if directory_entry_index >= scope_len {
                return Err(filemark_scan_error(format!(
                    "sidecar directory entry {} lies outside directory scope {}",
                    directory_entry.tape_file_number, directory.directory_scope_tape_file_count
                )));
            }
            if entry.block_count != directory_entry.sidecar_total_block_count {
                return Err(filemark_scan_error(format!(
                    "sidecar directory entry {} has block_count {}, scanned {}",
                    directory_entry.tape_file_number,
                    directory_entry.sidecar_total_block_count,
                    entry.block_count
                )));
            }
            if matches!(
                entry.kind,
                TapeFileKind::Bootstrap
                    | TapeFileKind::ParityMap
                    | TapeFileKind::TapeIndexReplica
                    | TapeFileKind::IndexSeparationExtent
            ) {
                return Err(filemark_scan_error(format!(
                    "sidecar directory entry {} conflicts with scanned {:?} control file",
                    directory_entry.tape_file_number, entry.kind
                )));
            }
            overlayed_entries.push(TapeFileMapEntry::parity_sidecar(
                directory_entry.tape_file_number,
                directory_entry.sidecar_total_block_count,
                directory_entry.epoch_id,
                directory_entry.protected_ordinal_start,
                directory_entry.protected_ordinal_end_exclusive,
            ));
            continue;
        }

        if entry.kind == TapeFileKind::Object {
            overlayed_entries.push(TapeFileMapEntry::object(
                entry.tape_file_number,
                entry.block_count,
                next_object_ordinal,
            ));
            next_object_ordinal = next_object_ordinal
                .checked_add(entry.block_count)
                .ok_or_else(|| filemark_scan_error("directory overlay object ordinals overflow"))?;
        } else {
            overlayed_entries.push(entry.clone());
        }
    }

    FilemarkMap::new(overlayed_entries)
}

fn validate_catalog_scope(
    catalog: &CatalogFilemarkMapInput,
    authoritative_bootstrap: &BootstrapPayload,
) -> Result<(), ParityError> {
    if catalog.tape_uuid != authoritative_bootstrap.tape_uuid {
        return Err(filemark_scan_error(
            "catalog tape UUID does not match authoritative bootstrap tape UUID",
        ));
    }

    let total_data_ordinals = catalog.map.total_data_ordinals();
    let highest_protected_ordinal = catalog.highest_protected_ordinal;
    if highest_protected_ordinal > total_data_ordinals {
        return Err(filemark_scan_error(format!(
            "catalog protection watermark {highest_protected_ordinal} exceeds total data ordinals {total_data_ordinals}"
        )));
    }

    let sidecar_watermark = catalog.map.max_sidecar_end_exclusive();
    if sidecar_watermark != highest_protected_ordinal {
        return Err(filemark_scan_error(format!(
            "catalog protection watermark {highest_protected_ordinal} does not match sidecar watermark {sidecar_watermark}"
        )));
    }

    Ok(())
}

fn filemark_scan_error(message: impl Into<String>) -> ParityError {
    ParityError::FilemarkMapReconstruct(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::{write_bootstrap_block, BootstrapPayload, ParitySchemeRecord};
    use crate::filemark_map::{FilemarkMapDigest, TapeFileKind, TapeFileMapEntry};
    use crate::model::{ParityScheme, SchemeId};
    use crate::parity_map::{
        encode_parity_map_tape_file, EncodedParityMapTapeFile, ParityMapPayload,
        SidecarEpochDirectory, SidecarEpochDirectoryEntry,
    };
    use crate::tape_index_replica::{
        checked_tape_index_replica_layout, plan_tape_index_edition, plan_tape_index_replica,
        write_tape_index_replica, TapeIndexEditionDescriptor, TapeIndexReplicaObservation,
    };
    use crate::terminal_tail::TerminalTailLayout;
    use crate::{
        TapeIndexReplicaCounts, TapeIndexReplicaFileKind, TapeIndexReplicaMapEntry,
        TapeIndexReplicaObjectRow, TapeIndexReplicaRecordSource, TapeIndexReplicaScope,
    };

    const BLOCK_SIZE: u32 = 512;
    const TAPE_UUID: [u8; 16] = [0x42; 16];

    fn block(seed: u8) -> Vec<u8> {
        vec![seed; BLOCK_SIZE as usize]
    }

    #[derive(Clone)]
    struct TerminalScanRows;

    impl TapeIndexReplicaRecordSource for TerminalScanRows {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            visitor(&TapeIndexReplicaMapEntry {
                tape_file_number: 0,
                kind: TapeIndexReplicaFileKind::Bootstrap,
                block_count: 1,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            })
        }

        fn visit_object_rows(
            &mut self,
            _visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            Ok(())
        }
    }

    fn terminal_replica_blocks() -> Vec<Vec<u8>> {
        let block_size = 256 * 1024;
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: 1,
            object_row_count: 0,
        };
        let records = checked_tape_index_replica_layout(block_size, counts)
            .unwrap()
            .replica_record_count;
        let layout = TerminalTailLayout::new(0, block_size, 1, 2, records, 2).unwrap();
        let mut rows = TerminalScanRows;
        let edition = plan_tape_index_edition(
            TapeIndexEditionDescriptor {
                tape_uuid: TAPE_UUID,
                edition_id: [0x73; 16],
                edition_sequence: 1,
                scope: TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 1,
                    total_data_ordinals: 0,
                    highest_protected_ordinal: 0,
                },
                counts,
                block_size,
                compression_enabled: false,
                writer_version: "scan-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: layout,
            },
            &mut rows,
        )
        .unwrap();
        let plan = plan_tape_index_replica(edition, 1).unwrap();
        let mut blocks = Vec::new();
        write_tape_index_replica(
            &plan,
            TapeIndexReplicaObservation {
                tape_file_number: 1,
                start_lba: 2,
                record_count: records,
            },
            &mut rows,
            |block| {
                blocks.push(block.to_vec());
                Ok(())
            },
        )
        .unwrap();
        blocks
    }

    #[test]
    fn terminal_replica_never_consumes_object_ordinals_even_with_damaged_header() {
        for damage_header in [false, true] {
            let mut replica = terminal_replica_blocks();
            if damage_header {
                let last = replica[0].len() - 1;
                replica[0][last] ^= 0x80;
            }
            let block_size = 256 * 1024;
            let mut bot = vec![0; block_size as usize];
            write_bootstrap_block(
                &BootstrapPayload {
                    scheme: None,
                    no_parity_flag: true,
                    filemark_map_digest: None,
                    tape_uuid: TAPE_UUID,
                    written_by_version: "scan-test".to_string(),
                    written_at: String::new(),
                    sequence: 0,
                    block_size_bytes: block_size,
                    drive_compression: false,
                },
                &mut bot,
            )
            .expect("BOT Bootstrap");
            let mut records = vec![
                Record::Block(bot),
                Record::Filemark,
                Record::Block(vec![0xA5; block_size as usize]),
                Record::Filemark,
            ];
            records.extend(replica.into_iter().map(Record::Block));
            records.push(Record::Filemark);
            let mut source = RecordingRawSource::new(records);
            let report =
                scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, block_size)
                    .expect("terminal control scan");
            assert_eq!(report.map.entries().len(), 3);
            assert_eq!(report.map.entries()[0].kind, TapeFileKind::Bootstrap);
            assert_eq!(report.map.entries()[1].kind, TapeFileKind::Object);
            assert_eq!(report.map.entries()[2].kind, TapeFileKind::TapeIndexReplica);
            assert_eq!(report.map.total_data_ordinals(), 1);
            assert_eq!(
                report
                    .damaged_regions
                    .iter()
                    .any(|region| region.kind == ScanDamageKind::InvalidTerminalControl),
                damage_header
            );
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Record {
        Block(Vec<u8>),
        Filemark,
        ReadFault(TestReadFault),
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum TestReadFault {
        Medium,
        DeferredFixedMedium,
        DeferredDescriptorMedium,
        Hardware,
        Transport,
    }

    impl TestReadFault {
        fn error(self) -> ParityError {
            ParityError::TapeIo(match self {
                Self::Medium => TapeIoError::CheckCondition(
                    remanence_library::scsi::ScsiError::CheckCondition {
                        sense: vec![0x72, 0x03, 0x11, 0x00],
                        bytes_transferred: 0,
                    },
                ),
                Self::DeferredFixedMedium => TapeIoError::CheckCondition(
                    remanence_library::scsi::ScsiError::CheckCondition {
                        sense: vec![0x71, 0x00, 0x03, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x11, 0],
                        bytes_transferred: 0,
                    },
                ),
                Self::DeferredDescriptorMedium => TapeIoError::CheckCondition(
                    remanence_library::scsi::ScsiError::CheckCondition {
                        sense: vec![0x73, 0x03, 0x11, 0x00],
                        bytes_transferred: 0,
                    },
                ),
                Self::Hardware => TapeIoError::CheckCondition(
                    remanence_library::scsi::ScsiError::CheckCondition {
                        sense: vec![0x72, 0x04, 0x44, 0x00],
                        bytes_transferred: 0,
                    },
                ),
                Self::Transport => {
                    TapeIoError::Transport(remanence_library::scsi::ScsiError::TransportError {
                        status: 0,
                        host_status: 0,
                        driver_status: 0x06,
                        info: 1,
                        sense: Vec::new(),
                    })
                }
            })
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum ScanCall {
        Configure(u32),
        Locate(u64),
        Position(u64),
        ReadRecord(u64),
        SpaceFilemarks(i64),
    }

    #[derive(Debug)]
    struct RecordingRawSource {
        records: Vec<Record>,
        cursor: usize,
        calls: Vec<ScanCall>,
    }

    impl RecordingRawSource {
        fn new(records: Vec<Record>) -> Self {
            Self {
                records,
                cursor: 0,
                calls: Vec::new(),
            }
        }
    }

    impl RawTapeSource for RecordingRawSource {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            self.calls.push(ScanCall::Configure(block_size));
            if block_size == 0 {
                return Err(ParityError::Invariant("test block size is zero"));
            }
            Ok(())
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.calls.push(ScanCall::Locate(hint.lba));
            self.cursor = usize::try_from(hint.lba)
                .map_err(|_| ParityError::Invariant("test LBA does not fit usize"))?
                .min(self.records.len());
            Ok(())
        }

        fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.cursor = self.records.len();
            Ok(PhysicalPositionHint::new(self.cursor as u64))
        }

        fn space_filemarks(
            &mut self,
            count: i64,
        ) -> Result<crate::SpaceFilemarksOutcome, ParityError> {
            self.calls.push(ScanCall::SpaceFilemarks(count));
            if count < 0 {
                return Err(ParityError::Invariant(
                    "test source only spaces filemarks forward",
                ));
            }

            let mut spaced = 0i64;
            while self.cursor < self.records.len() && spaced < count {
                let is_filemark = matches!(self.records[self.cursor], Record::Filemark);
                self.cursor += 1;
                if is_filemark {
                    spaced += 1;
                }
            }

            Ok(crate::SpaceFilemarksOutcome {
                filemarks_spaced: spaced,
                position_after: PhysicalPositionHint::new(self.cursor as u64),
                hit_end_of_data: spaced < count,
            })
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            self.calls.push(ScanCall::ReadRecord(self.cursor as u64));
            let Some(record) = self.records.get(self.cursor) else {
                return Ok(RawReadOutcome::EndOfData {
                    position_after: PhysicalPositionHint::new(self.cursor as u64),
                });
            };

            match record {
                Record::Block(block) => {
                    if block.len() > buf.len() {
                        self.cursor += 1;
                        return Err(remanence_library::TapeIoError::ReadBufferTooSmall {
                            actual: block.len() as u32,
                            provided: buf.len() as u32,
                        }
                        .into());
                    }
                    let bytes = block.len();
                    buf[..bytes].copy_from_slice(block);
                    self.cursor += 1;
                    Ok(RawReadOutcome::Block {
                        bytes,
                        position_after: PhysicalPositionHint::new(self.cursor as u64),
                    })
                }
                Record::Filemark => {
                    self.cursor += 1;
                    Ok(RawReadOutcome::Filemark {
                        position_after: PhysicalPositionHint::new(self.cursor as u64),
                    })
                }
                Record::ReadFault(fault) => Err(fault.error()),
            }
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.calls.push(ScanCall::Position(self.cursor as u64));
            Ok(PhysicalPositionHint::new(self.cursor as u64))
        }
    }

    #[test]
    fn controlled_walk_reports_each_file_and_aborts_before_reading_the_next() {
        let mut bot = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(
            &BootstrapPayload {
                scheme: None,
                no_parity_flag: true,
                filemark_map_digest: None,
                tape_uuid: TAPE_UUID,
                written_by_version: "controlled-scan-test".to_string(),
                written_at: String::new(),
                sequence: 0,
                block_size_bytes: BLOCK_SIZE,
                drive_compression: false,
            },
            &mut bot,
        )
        .expect("BOT Bootstrap");
        let records = vec![
            Record::Block(bot),
            Record::Filemark,
            Record::Block(block(0x22)),
            Record::Filemark,
        ];
        let mut source = RecordingRawSource::new(records.clone());
        let mut progress = Vec::new();
        let outcome = scan_reconstruct_filemark_map_with_control(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            |event| {
                progress.push(*event);
                ScanWalkControl::Abort
            },
        )
        .expect("controlled BOT walk");

        let ControlledScanWalkOutcome::Aborted(aborted) = outcome else {
            panic!("the controller must stop the walk")
        };
        assert_eq!(progress.len(), 1);
        assert_eq!(progress[0].tape_file_number, 0);
        assert_eq!(progress[0].position, PhysicalPositionHint::new(2));
        assert_eq!(progress[0].structural_candidate_count, 1);
        assert_eq!(aborted.last_tape_file_number, 0);
        assert_eq!(aborted.position, PhysicalPositionHint::new(2));
        assert_eq!(aborted.structural_candidate_count, 1);
        assert!(
            !source.calls.contains(&ScanCall::ReadRecord(2)),
            "abort at file 0 must occur before reading file 1"
        );

        let mut complete_source = RecordingRawSource::new(records);
        let mut complete_progress = Vec::new();
        let complete = scan_reconstruct_filemark_map_with_control(
            &mut complete_source,
            &TAPE_UUID,
            BLOCK_SIZE,
            |event| {
                complete_progress.push(*event);
                ScanWalkControl::Continue
            },
        )
        .expect("complete controlled BOT walk");
        let ControlledScanWalkOutcome::Complete(complete) = complete else {
            panic!("continue controller must reach EOD")
        };
        assert_eq!(complete.map.tape_file_count(), 2);
        assert_eq!(complete_progress.len(), 2);
        assert_eq!(complete_progress[0].tape_file_number, 0);
        assert_eq!(complete_progress[1].tape_file_number, 1);
        assert_eq!(complete_progress[1].structural_candidate_count, 2);
    }

    fn sample_scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("test"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
        }
    }

    fn sample_scheme_record() -> ParitySchemeRecord {
        ParitySchemeRecord {
            id: sample_scheme().id.as_str().to_string(),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 1,
            no_parity_flag: false,
        }
    }

    fn bootstrap_payload(digest: FilemarkMapDigest, sequence: u64) -> BootstrapPayload {
        BootstrapPayload {
            scheme: Some(sample_scheme_record()),
            no_parity_flag: false,
            filemark_map_digest: Some(digest),
            tape_uuid: TAPE_UUID,
            written_by_version: "scan-test".to_string(),
            written_at: String::new(),
            sequence,
            block_size_bytes: BLOCK_SIZE,
            drive_compression: false,
        }
    }

    fn bootstrap_block(digest: FilemarkMapDigest, sequence: u64) -> Vec<u8> {
        let payload = bootstrap_payload(digest, sequence);
        let mut block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&payload, &mut block).expect("bootstrap block encodes");
        block
    }

    #[test]
    fn acquire_filemark_map_refuses_compressed_parity_bootstrap() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
            .expect("bootstrap-only map validates");
        let mut payload = bootstrap_payload(map.digest(false).expect("digest builds"), 0);
        payload.drive_compression = true;
        let mut source = RecordingRawSource::new(Vec::new());

        let err = acquire_filemark_map(&mut source, &payload, None)
            .expect_err("compressed parity bootstrap must disable 3c recovery");

        assert!(matches!(err, ParityError::DriveCompressionEnabled));
        assert!(
            source.calls.is_empty(),
            "compression rejection must happen before scan I/O"
        );
    }

    fn bootstrap_block_for_payload(payload: &BootstrapPayload) -> Vec<u8> {
        let mut block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(payload, &mut block).expect("bootstrap block encodes");
        block
    }

    #[test]
    fn empty_tail_file_terminates_walk_with_complete_prefix() {
        let expected_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
            .expect("BOT-only map validates");
        let bot = bootstrap_block(expected_map.digest(false).expect("BOT digest"), 0);
        let mut records = vec![Record::Block(bot), Record::Filemark];
        let empty_file_position = PhysicalPositionHint::new(records.len() as u64);
        records.push(Record::Filemark);
        let mut source = RecordingRawSource::new(records);

        let walk = scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("empty tail file terminates rather than aborting the walk");

        assert_eq!(walk.map, expected_map);
        assert_eq!(
            walk.truncation,
            Some(ScanTailTruncation {
                tape_file_number: expected_map.tape_file_count(),
                position: empty_file_position,
                kind: ScanTailTruncationKind::EmptyFile,
            })
        );
    }

    #[test]
    fn zero_block_measurement_is_a_tail_truncation_signature() {
        let mut source = RecordingRawSource::new(vec![Record::Filemark]);

        let measured = measure_current_file(&mut source, PhysicalPositionHint::new(0))
            .expect("zero-block measurement is classified");

        assert_eq!(
            measured,
            MeasureCurrentFileOutcome::Truncated(ScanTailTruncationKind::ZeroBlockFile)
        );
    }

    #[test]
    fn scanner_degrades_only_medium_errors_at_tape_file_heads() {
        let bot_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
            .expect("BOT-only map validates");
        let bot = bootstrap_block(bot_map.digest(false).expect("BOT digest"), 0);
        let mut medium_source = RecordingRawSource::new(vec![
            Record::Block(bot.clone()),
            Record::Filemark,
            Record::ReadFault(TestReadFault::Medium),
            Record::Filemark,
        ]);

        let walked =
            scan_reconstruct_filemark_map_with_report(&mut medium_source, &TAPE_UUID, BLOCK_SIZE)
                .expect("a SCSI medium error is retained as physical damage");
        assert_eq!(walked.map.entries()[1].kind, TapeFileKind::Object);
        assert!(walked.damaged_regions.iter().any(|region| {
            region.start.lba == 2 && region.kind == ScanDamageKind::UnreadableTapeFileHead
        }));

        for fault in [
            TestReadFault::DeferredFixedMedium,
            TestReadFault::DeferredDescriptorMedium,
            TestReadFault::Hardware,
            TestReadFault::Transport,
        ] {
            let mut source = RecordingRawSource::new(vec![
                Record::Block(bot.clone()),
                Record::Filemark,
                Record::ReadFault(fault),
                Record::Filemark,
            ]);
            let error =
                scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
                    .expect_err("non-medium head failures must abort the structural walk");
            match fault {
                TestReadFault::DeferredFixedMedium
                | TestReadFault::DeferredDescriptorMedium
                | TestReadFault::Hardware => assert!(matches!(
                    error,
                    ParityError::TapeIo(TapeIoError::CheckCondition(_))
                )),
                TestReadFault::Transport => assert!(matches!(
                    error,
                    ParityError::TapeIo(TapeIoError::Transport(_))
                )),
                TestReadFault::Medium => unreachable!("current-medium case was tested separately"),
            }
        }
    }

    #[test]
    fn scanner_degrades_only_medium_errors_during_optional_tail_probes() {
        let bot_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
            .expect("BOT-only map validates");
        let bot = bootstrap_block(bot_map.digest(false).expect("BOT digest"), 0);
        let records_for = |fault| {
            vec![
                Record::Block(bot.clone()),
                Record::Filemark,
                Record::Block(block(0xA5)),
                Record::ReadFault(fault),
                Record::Filemark,
            ]
        };
        let mut medium_source = RecordingRawSource::new(records_for(TestReadFault::Medium));

        let walked =
            scan_reconstruct_filemark_map_with_report(&mut medium_source, &TAPE_UUID, BLOCK_SIZE)
                .expect("a medium-damaged optional footer remains unclassified Object evidence");
        assert_eq!(walked.map.entries()[1].kind, TapeFileKind::Object);
        assert_eq!(walked.map.entries()[1].block_count, 2);

        for fault in [
            TestReadFault::DeferredFixedMedium,
            TestReadFault::DeferredDescriptorMedium,
            TestReadFault::Hardware,
            TestReadFault::Transport,
        ] {
            let mut source = RecordingRawSource::new(records_for(fault));
            let error =
                scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
                    .expect_err(
                        "non-medium optional-probe failures must abort the structural walk",
                    );
            match fault {
                TestReadFault::DeferredFixedMedium
                | TestReadFault::DeferredDescriptorMedium
                | TestReadFault::Hardware => assert!(matches!(
                    error,
                    ParityError::TapeIo(TapeIoError::CheckCondition(_))
                )),
                TestReadFault::Transport => assert!(matches!(
                    error,
                    ParityError::TapeIo(TapeIoError::Transport(_))
                )),
                TestReadFault::Medium => unreachable!("current-medium case was tested separately"),
            }
        }
    }

    fn encode_test_parity_map(
        sequence: u64,
        directory: SidecarEpochDirectory,
        canonical_map_digest: [u8; 32],
        writer_version: &str,
    ) -> EncodedParityMapTapeFile {
        encode_parity_map_tape_file(
            &ParityMapPayload {
                tape_uuid: TAPE_UUID,
                sequence,
                directory,
                canonical_map_digest,
                writer_version: Some(writer_version.to_string()),
                write_timestamp: None,
            },
            BLOCK_SIZE,
        )
        .expect("test parity_map encodes")
    }

    fn synthetic_directory_entry(
        tape_file_number: u64,
        epoch_id: u64,
    ) -> SidecarEpochDirectoryEntry {
        SidecarEpochDirectoryEntry {
            tape_file_number,
            epoch_id,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: 1,
            sidecar_total_block_count: 1,
            sidecar_header_block_count: 1,
            parity_shard_block_count: 1,
            canonical_metadata_hash: [epoch_id as u8; 32],
            flags: 0,
        }
    }

    fn ambiguous_structural_parity_map_fixture(
        first_sequence: u64,
        second_sequence: u64,
    ) -> (Vec<Record>, FilemarkMap, FilemarkMap, BootstrapPayload) {
        let first_directory = SidecarEpochDirectory {
            directory_scope_tape_file_count: 6,
            directory_scope_total_data_ordinals: 2,
            directory_scope_highest_protected_ordinal: 1,
            is_final_directory: true,
            entries: vec![synthetic_directory_entry(1, 0)],
        };
        let second_directory = SidecarEpochDirectory {
            directory_scope_tape_file_count: 6,
            directory_scope_total_data_ordinals: 2,
            directory_scope_highest_protected_ordinal: 1,
            is_final_directory: true,
            entries: vec![synthetic_directory_entry(3, 0)],
        };
        let provisional_first =
            encode_test_parity_map(first_sequence, first_directory.clone(), [0; 32], "first");
        let provisional_second =
            encode_test_parity_map(second_sequence, second_directory.clone(), [0; 32], "second");
        let first_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::parity_sidecar(1, 1, 0, 0, 1),
            TapeFileMapEntry::parity_map(2, provisional_first.blocks.len() as u64),
            TapeFileMapEntry::object(3, 1, 0),
            TapeFileMapEntry::parity_map(4, provisional_second.blocks.len() as u64),
            TapeFileMapEntry::object(5, 1, 1),
        ])
        .expect("first ambiguous projection validates");
        let second_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 1, 0),
            TapeFileMapEntry::parity_map(2, provisional_first.blocks.len() as u64),
            TapeFileMapEntry::parity_sidecar(3, 1, 0, 0, 1),
            TapeFileMapEntry::parity_map(4, provisional_second.blocks.len() as u64),
            TapeFileMapEntry::object(5, 1, 1),
        ])
        .expect("second ambiguous projection validates");
        let first_parity_map = encode_test_parity_map(
            first_sequence,
            first_directory,
            first_map.canonical_digest().expect("first digest builds"),
            "first",
        );
        let second_parity_map = encode_test_parity_map(
            second_sequence,
            second_directory,
            second_map.canonical_digest().expect("second digest builds"),
            "second",
        );
        assert_eq!(
            first_parity_map.blocks.len(),
            provisional_first.blocks.len()
        );
        assert_eq!(
            second_parity_map.blocks.len(),
            provisional_second.blocks.len()
        );

        let prefix_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
            .expect("prefix map validates");
        let authoritative_bootstrap =
            bootstrap_payload(prefix_map.digest(false).expect("prefix digest builds"), 0);
        let mut records = vec![
            Record::Block(bootstrap_block_for_payload(&authoritative_bootstrap)),
            Record::Filemark,
            Record::Block(block(0xB1)),
            Record::Filemark,
        ];
        records.extend(first_parity_map.blocks.into_iter().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(block(0xB2)),
            Record::Filemark,
        ]);
        records.extend(second_parity_map.blocks.into_iter().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::ReadFault(TestReadFault::Medium),
            Record::Filemark,
        ]);
        (records, first_map, second_map, authoritative_bootstrap)
    }

    #[test]
    fn structural_parity_map_ranking_prefers_greatest_sequence() {
        let (records, first_map, second_map, authoritative_bootstrap) =
            ambiguous_structural_parity_map_fixture(4, 5);
        assert_ne!(first_map, second_map, "fixture projections must disagree");

        let mut source = RecordingRawSource::new(records);
        let result = acquire_filemark_map_with_report(&mut source, &authoritative_bootstrap, None)
            .expect("both parity_maps validate before ranking");
        assert_eq!(result.scoped_map.map, second_map);
        assert!(result.parity_map_content_conflicts.is_empty());
    }

    #[test]
    fn structural_parity_map_equal_key_uses_lowest_file_and_reports_conflict() {
        let (records, first_map, second_map, authoritative_bootstrap) =
            ambiguous_structural_parity_map_fixture(7, 7);
        assert_ne!(first_map, second_map, "fixture projections must disagree");

        let mut source = RecordingRawSource::new(records);
        let result = acquire_filemark_map_with_report(&mut source, &authoritative_bootstrap, None)
            .expect("equal-key content disagreement is non-fatal");
        assert_eq!(result.scoped_map.map, first_map);
        assert_eq!(
            result.parity_map_content_conflicts,
            vec![ParityMapContentConflict {
                candidate_tape_file_numbers: vec![2, 4],
                selection_key: ParityMapSelectionKey {
                    is_final_directory: true,
                    sequence: 7,
                    directory_scope_total_data_ordinals: 2,
                },
                chosen_tape_file_number: 2,
            }]
        );
    }

    #[test]
    fn scanner_admits_bootstrap_only_at_bot() {
        let bot_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
            .expect("BOT-only map validates");
        let bot = bootstrap_block(bot_map.digest(false).expect("BOT digest"), 0);
        let records = vec![
            Record::Block(bot.clone()),
            Record::Filemark,
            Record::Block(bot),
            Record::Filemark,
        ];
        let mut source = RecordingRawSource::new(records);
        let walked = scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("structural scan succeeds");

        assert_eq!(
            walked
                .map
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![TapeFileKind::Bootstrap, TapeFileKind::Object]
        );
        assert_eq!(walked.bootstrap_candidates.len(), 1);
        assert_eq!(walked.bootstrap_candidates[0].tape_file_number, 0);
    }

    #[test]
    fn truncated_later_bootstrap_is_an_object_candidate() {
        let bot_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
            .expect("BOT-only map validates");
        let bot = bootstrap_block(bot_map.digest(false).expect("BOT digest"), 0);
        let records = vec![
            Record::Block(bot.clone()),
            Record::Filemark,
            Record::Block(bot),
        ];
        let mut source = RecordingRawSource::new(records);
        let walked = scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("torn-tail scan succeeds");

        assert_eq!(walked.map.tape_file_count(), 1);
        assert_eq!(walked.truncation_candidate_kind, Some(TapeFileKind::Object));
        assert_eq!(walked.bootstrap_candidates.len(), 1);
    }
}
