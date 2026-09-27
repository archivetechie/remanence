//! Catalog-less filemark-map reconstruction for Layer 3c v0.4.4.
//!
//! The scanner walks physical tape files from BOT, reads only the first block
//! of each file for structural classification, and measures file length by
//! spacing to the next filemark. Bootstrap, parity-map, and sidecar tape files
//! are accepted only after their magic plus CRC/header validation succeeds.
//! Terminal replica/separation magic is structurally reserved: damaged terminal
//! framing remains typed control evidence so it cannot consume Object ordinals.

use crate::bootstrap::{has_bootstrap_magic, parse_bootstrap_block, BootstrapPayload};
use crate::error::ParityError;
use crate::filemark_map::{
    FilemarkMap, FilemarkMapBuilder, ScopedFilemarkMap, TapeFileKind, TapeFileMapEntry,
    TapeFilePosition,
};
use crate::index_separation::{
    derive_index_separation_footer_magic, derive_index_separation_header_magic,
    parse_index_separation_footer, parse_index_separation_header,
};
use crate::parity_map::classify_parity_map_header_block;
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

/// Complete out-of-band authority for an unreadable BOT Bootstrap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanRecoveryHints {
    /// Supplied tape identity, also used to derive role magics.
    pub tape_uuid: [u8; 16],
    /// Supplied fixed-block geometry.
    pub block_size: u32,
    /// Explicit parity geometry or an explicit no-parity declaration.
    pub scheme: crate::ParityConfig,
}

/// Recovery classification after binding all trustworthy BOT evidence to hints.
#[derive(Clone, Debug)]
pub enum RecoveryBootstrap {
    /// The complete bootstrap validates and agrees with the supplied authority.
    Validated(BootstrapPayload),
    /// BOT damage or nonconformance supplies no conflicting validated evidence.
    Unreadable(String),
}

impl ScanRecoveryHints {
    /// Classify a recovery BOT block, refusing validated disagreements before
    /// parsing its payload. Physical read failures are handled by the caller.
    pub fn classify_bootstrap(&self, block: &[u8]) -> Result<RecoveryBootstrap, ParityError> {
        let header_error = if block.len() < crate::bootstrap::BOOTSTRAP_HEADER_LEN {
            Some("bootstrap header buffer too short")
        } else if block[..8] != crate::bootstrap::BOOTSTRAP_MAGIC {
            Some("bootstrap magic mismatch")
        } else if crate::crc64_xz(&block[..48])
            != u64::from_le_bytes(block[48..56].try_into().expect("header CRC bytes"))
        {
            Some("bootstrap header CRC mismatch")
        } else {
            None
        };
        if let Some(reason) = header_error {
            return Ok(RecoveryBootstrap::Unreadable(reason.into()));
        }
        let major = u16::from_be_bytes(block[8..10].try_into().expect("header schema bytes"));
        if major != crate::bootstrap::BOOTSTRAP_SCHEMA_MAJOR {
            return Err(filemark_scan_error(format!(
                "unsupported bootstrap schema major version: got {major}, accept {}",
                crate::bootstrap::BOOTSTRAP_SCHEMA_MAJOR
            )));
        }
        if block[16..32] != self.tape_uuid {
            return Err(ParityError::TapeIdentityMismatch(
                "tape identity mismatch: readable bootstrap header differs from supplied hints"
                    .into(),
            ));
        }
        if u32::from_be_bytes(block[32..36].try_into().expect("header block size bytes"))
            != self.block_size
        {
            return Err(filemark_scan_error(
                "readable bootstrap block size differs from supplied hints",
            ));
        }
        let sequence = u64::from_be_bytes(block[36..44].try_into().expect("header sequence bytes"));
        if sequence != 0 {
            return Err(filemark_scan_error(format!(
                "schema-major 2 permits only the sequence-0 BOT Bootstrap: got sequence {sequence}"
            )));
        }
        let flags = u32::from_be_bytes(block[12..16].try_into().expect("header flags bytes"));
        if (flags & crate::bootstrap::FLAG_NO_PARITY != 0)
            != matches!(self.scheme, crate::ParityConfig::None)
        {
            return Err(filemark_scan_error(
                "readable bootstrap parity scheme differs from supplied hints: no-parity flag contradicts scheme",
            ));
        }
        if block.len() != self.block_size as usize {
            return Err(filemark_scan_error(format!(
                "readable bootstrap block size differs from supplied hints: got {} bytes, expected {}",
                block.len(),
                self.block_size
            )));
        }
        match parse_bootstrap_block(block) {
            Ok(payload) => {
                self.validate_bootstrap(&payload)?;
                Ok(RecoveryBootstrap::Validated(payload))
            }
            Err(error @ ParityError::BootstrapParse(_)) => {
                // Bounds are covered by the header CRC. An impossible length is
                // nonconformance, but cannot supply authenticated payload fields.
                let payload_len = u32::from_le_bytes(block[44..48].try_into().unwrap());
                let payload_end = usize::try_from(payload_len)
                    .ok()
                    .and_then(|len| crate::bootstrap::BOOTSTRAP_HEADER_LEN.checked_add(len));
                let framed_end = payload_end.and_then(|end| end.checked_add(8));
                let Some((end, crc_end)) = payload_end
                    .zip(framed_end)
                    .filter(|(_, crc_end)| *crc_end <= block.len())
                else {
                    return Ok(RecoveryBootstrap::Unreadable(format!(
                        "bootstrap payload is readable but nonconformant: {error}"
                    )));
                };
                let bytes = &block[crate::bootstrap::BOOTSTRAP_HEADER_LEN..end];
                let stored_crc = u64::from_le_bytes(block[end..crc_end].try_into().unwrap());
                if crate::crc64_xz(bytes) != stored_crc {
                    return Ok(RecoveryBootstrap::Unreadable(error.to_string()));
                }
                // Decode without canonical/order/digest/legacy-key validation:
                // these rules must not hide independently readable conflicts.
                use ciborium::value::Value;
                if let Ok(Value::Map(entries)) = ciborium::from_reader::<Value, _>(bytes) {
                    for (key, value) in entries {
                        if key == Value::Integer(5.into()) && value == Value::Bool(true) {
                            return Err(ParityError::DriveCompressionEnabled);
                        }
                        if key != Value::Integer(1.into()) {
                            continue;
                        }
                        let Value::Map(fields) = value else { continue };
                        let field = |key: u8| {
                            fields
                                .iter()
                                .find(|(k, _)| *k == Value::Integer(key.into()))
                                .map(|(_, v)| v)
                        };
                        let decoded = (|| {
                            let id = field(1)?.as_text()?;
                            let k = u16::try_from(i128::from(field(2)?.as_integer()?)).ok()?;
                            let m = u16::try_from(i128::from(field(3)?.as_integer()?)).ok()?;
                            let s = u32::try_from(i128::from(field(4)?.as_integer()?)).ok()?;
                            Some((id, k, m, s))
                        })();
                        if let Some((id, k, m, s)) = decoded {
                            let agrees = match &self.scheme {
                                crate::ParityConfig::None => false,
                                crate::ParityConfig::Scheme(expected) => {
                                    expected.id.as_str() == id
                                        && expected.data_blocks_per_stripe == k
                                        && expected.parity_blocks_per_stripe == m
                                        && expected.stripes_per_neighborhood == s
                                }
                            };
                            if !agrees {
                                return Err(filemark_scan_error(
                                    "readable bootstrap parity scheme differs from supplied hints",
                                ));
                            }
                        }
                    }
                }
                Ok(RecoveryBootstrap::Unreadable(format!(
                    "bootstrap payload is readable but nonconformant: {error}"
                )))
            }
            Err(error) => Err(error),
        }
    }

    /// Refuse any disagreement with a readable, valid bootstrap.
    fn validate_bootstrap(&self, bootstrap: &BootstrapPayload) -> Result<(), ParityError> {
        if self.tape_uuid != bootstrap.tape_uuid {
            return Err(ParityError::TapeIdentityMismatch(
                "tape identity mismatch: readable bootstrap differs from supplied hints".into(),
            ));
        }
        if self.block_size != bootstrap.block_size_bytes {
            return Err(filemark_scan_error(
                "readable bootstrap block size differs from supplied hints",
            ));
        }
        let matches = match (&self.scheme, &bootstrap.scheme) {
            (crate::ParityConfig::None, _) => bootstrap.no_parity_flag,
            (crate::ParityConfig::Scheme(expected), Some(actual)) => {
                !bootstrap.no_parity_flag
                    && expected.id.as_str() == actual.id
                    && expected.data_blocks_per_stripe == actual.data_blocks_per_stripe
                    && expected.parity_blocks_per_stripe == actual.parity_blocks_per_stripe
                    && expected.stripes_per_neighborhood == actual.stripes_per_neighborhood
            }
            _ => false,
        };
        if !matches {
            return Err(filemark_scan_error(
                "readable bootstrap parity scheme differs from supplied hints",
            ));
        }
        Ok(())
    }
}

/// Scoped recovery opt-in; ordinary callers preserve the original scanner rules.
#[derive(Clone, Copy, Debug, Default)]
pub enum ScanMode<'a> {
    /// Require the existing structural bootstrap validation.
    #[default]
    Standard,
    /// Treat an invalid or physically unreadable file-0 bootstrap as unreadable.
    Recovery(&'a ScanRecoveryHints),
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
    /// Physical damage or bootstrap validation failure encountered by the scanner.
    pub damaged_regions: Vec<ScanDamagedRegion>,
    /// Present only when file 0 was treated as unreadable and these supplied
    /// values provided the tape identity and geometry instead of a bootstrap.
    pub bootstrap_recovery_hints: Option<ScanRecoveryHints>,
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

/// Scanner-observed physical damage or bootstrap validation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanDamageKind {
    /// The first block was physically unreadable, or file 0 failed bootstrap
    /// validation under the explicit recovery mode.
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

/// Source that supplied the filemark map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanOverlaySource {
    /// The structural walk supplied the map.
    StructuralWalk,
    /// A catalog supplied the complete map.
    Catalog,
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
    /// Bootstrap sequence whose scope governed map validation.
    pub authoritative_bootstrap_sequence: u64,
    /// Source that supplied the map for validation.
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
/// `filemark_map_digest`, preserving the bootstrap's prefix scope.
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
/// This validates the bootstrap's digest scope without a second physical
/// tape walk.
pub fn validate_scan_reconstruction_with_report(
    _source: &mut dyn RawTapeSource,
    authoritative_bootstrap: &BootstrapPayload,
    reconstructed: ScanWalkResult,
) -> Result<FilemarkMapScanResult, ParityError> {
    let Some(digest) = authoritative_bootstrap.filemark_map_digest.as_ref() else {
        return Err(filemark_scan_error(
            "authoritative bootstrap does not carry a filemark-map digest",
        ));
    };
    match ScopedFilemarkMap::validate_against_digest(reconstructed.map, digest) {
        Ok(scoped_map) => filemark_map_scan_result(
            scoped_map,
            reconstructed.truncation,
            authoritative_bootstrap.sequence,
            ScanOverlaySource::StructuralWalk,
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
    scan_reconstruct_filemark_map_with_report_mode(
        source,
        tape_uuid,
        block_size,
        ScanMode::Standard,
    )
}

/// Reporting scan with an explicit, fully supplied bootstrap recovery mode.
pub fn scan_reconstruct_filemark_map_with_report_mode(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    mode: ScanMode<'_>,
) -> Result<ScanWalkResult, ParityError> {
    let outcome = scan_reconstruct_filemark_map_with_control_mode(
        source,
        tape_uuid,
        block_size,
        mode,
        |_| ScanWalkControl::Continue,
    )?;
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
    control: F,
) -> Result<ControlledScanWalkOutcome, ParityError>
where
    F: FnMut(&ScanWalkProgress) -> ScanWalkControl,
{
    scan_reconstruct_filemark_map_with_control_mode(
        source,
        tape_uuid,
        block_size,
        ScanMode::Standard,
        control,
    )
}

/// Controlled scan using the same walk with scoped bootstrap recovery.
pub fn scan_reconstruct_filemark_map_with_control_mode<F>(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    mode: ScanMode<'_>,
    mut control: F,
) -> Result<ControlledScanWalkOutcome, ParityError>
where
    F: FnMut(&ScanWalkProgress) -> ScanWalkControl,
{
    match scan_reconstruct_filemark_map_with_provenance(
        source,
        tape_uuid,
        block_size,
        mode,
        &mut control,
    )? {
        ScanReconstructionOutcome::Complete(reconstructed) => {
            Ok(ControlledScanWalkOutcome::Complete(ScanWalkResult {
                bootstrap_recovery_hints: reconstructed.bootstrap_recovery_hints,
                map: reconstructed.map,
                truncation: reconstructed.truncation,
                truncation_candidate_kind: reconstructed.truncation_candidate_kind,
                bootstrap_candidates: reconstructed.bootstrap_candidates,
                damaged_regions: reconstructed.damaged_regions,
            }))
        }
        ScanReconstructionOutcome::Aborted(aborted) => {
            Ok(ControlledScanWalkOutcome::Aborted(aborted))
        }
    }
}

#[derive(Debug)]
struct ScanReconstruction {
    bootstrap_recovery_hints: Option<ScanRecoveryHints>,
    map: FilemarkMap,
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
    mode: ScanMode<'_>,
    control: &mut F,
) -> Result<ScanReconstructionOutcome, ParityError>
where
    F: FnMut(&ScanWalkProgress) -> ScanWalkControl,
{
    if block_size == 0 {
        return Err(ParityError::Invariant("scan block size is zero"));
    }

    if let ScanMode::Recovery(hints) = mode {
        if hints.tape_uuid != *tape_uuid || hints.block_size != block_size {
            return Err(filemark_scan_error(
                "scan arguments disagree with recovery hints",
            ));
        }
        if let crate::ParityConfig::Scheme(scheme) = &hints.scheme {
            scheme.validate()?;
        }
    }
    let mut bootstrap_recovery_hints = None;
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| ParityError::Invariant("scan block size does not fit usize"))?;
    source.configure_fixed_block_size(block_size)?;
    source.locate_physical(PhysicalPositionHint::new(0))?;

    let mut builder = FilemarkMapBuilder::new();
    let mut buf = vec![0u8; block_size_usize];
    let mut saw_file = false;
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
                let mut invalid_bootstrap = false;
                if builder.next_tape_file_number()? == 0 && file_start.lba == 0 {
                    if let ScanMode::Recovery(hints) = mode {
                        match hints.classify_bootstrap(&first_block)? {
                            RecoveryBootstrap::Validated(_) => {}
                            RecoveryBootstrap::Unreadable(_) => {
                                invalid_bootstrap = true;
                                bootstrap_recovery_hints = Some(hints.clone());
                                damaged_regions.push(ScanDamagedRegion {
                                    start: file_start,
                                    block_count: 1,
                                    kind: ScanDamageKind::UnreadableTapeFileHead,
                                });
                            }
                        }
                    }
                }
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
                if invalid_bootstrap {
                    append_entry_with_unreadable_head(
                        source,
                        &mut builder,
                        tape_uuid,
                        block_size,
                        file_start,
                        measured.block_count,
                        &mut damaged_regions,
                    )?;
                } else if let Some(candidate) = append_classified_entry(
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
                if builder.next_tape_file_number()? == 0 && file_start.lba == 0 {
                    if let ScanMode::Recovery(hints) = mode {
                        bootstrap_recovery_hints = Some(hints.clone());
                    }
                }
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
                append_entry_with_unreadable_head(
                    source,
                    &mut builder,
                    tape_uuid,
                    block_size,
                    file_start,
                    measured.block_count,
                    &mut damaged_regions,
                )?;
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
        bootstrap_recovery_hints,
        map: builder.build()?,
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
    // reader needs the rest of the map for bootstrap digest validation.
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
    /// Opt-in recovery preserves the sole structural BOT without authenticating it.
    #[test]
    fn scanner_recovery_requires_hints_and_records_bootstrap_provenance() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT-only map");
        let valid = bootstrap_block(map.digest(false).expect("digest"), 0);
        let mut malformed = valid.clone();
        malformed[80] ^= 1;
        for scheme in [
            crate::ParityConfig::None,
            crate::ParityConfig::Scheme(crate::default_scheme_for_block_size(BLOCK_SIZE)),
        ] {
            let hints = ScanRecoveryHints {
                tape_uuid: TAPE_UUID,
                block_size: BLOCK_SIZE,
                scheme,
            };
            // Payload damage is recoverable only when the checked header agrees.
            let flags = u32::from(matches!(hints.scheme, crate::ParityConfig::None));
            malformed[12..16].copy_from_slice(&flags.to_be_bytes());
            let crc = crate::crc64_xz(&malformed[..48]);
            malformed[48..56].copy_from_slice(&crc.to_le_bytes());
            for head in [
                Record::Block(malformed.clone()),
                Record::Block(block(0)),
                Record::ReadFault(TestReadFault::Medium),
            ] {
                let records = vec![
                    head,
                    Record::Filemark,
                    Record::Block(block(0x33)),
                    Record::Filemark,
                ];
                let mut source = RecordingRawSource::new(records.clone());
                let report = scan_reconstruct_filemark_map_with_report_mode(
                    &mut source,
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    ScanMode::Recovery(&hints),
                )
                .expect("scoped recovery");
                assert_eq!(report.bootstrap_recovery_hints, Some(hints.clone()));
                assert!(report.authoritative_bootstrap().is_none());
                assert_eq!(report.map.entries()[0].kind, TapeFileKind::Bootstrap);
                assert_eq!(report.map.entries()[1].kind, TapeFileKind::Object);
                assert_eq!(report.damaged_regions.len(), 1);
                if matches!(records[0], Record::Block(_)) {
                    let error = scan_reconstruct_filemark_map_with_report(
                        &mut RecordingRawSource::new(records),
                        &TAPE_UUID,
                        BLOCK_SIZE,
                    )
                    .expect_err("without hints a malformed file 0 must refuse");
                    assert!(
                        error.to_string().contains("sole tape-file-0 BOT Bootstrap"),
                        "{error}"
                    );
                }
            }
        }
    }

    /// Scoped recovery cannot replace a valid bootstrap or conceal transport failures.
    #[test]
    fn scanner_recovery_refuses_valid_bootstrap_conflicts_and_transport_errors() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT map");
        let payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        let record = payload.scheme.as_ref().expect("parity bootstrap");
        let hints = ScanRecoveryHints {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            scheme: crate::ParityConfig::Scheme(ParityScheme {
                id: SchemeId::new_owned(record.id.clone()),
                data_blocks_per_stripe: record.data_blocks_per_stripe,
                parity_blocks_per_stripe: record.parity_blocks_per_stripe,
                stripes_per_neighborhood: record.stripes_per_neighborhood,
            }),
        };
        let records = vec![
            Record::Block(bootstrap_block_for_payload(&payload)),
            Record::Filemark,
        ];
        let report = scan_reconstruct_filemark_map_with_report_mode(
            &mut RecordingRawSource::new(records.clone()),
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Recovery(&hints),
        )
        .expect("matching hints leave bootstrap authoritative");
        assert_eq!(
            report
                .authoritative_bootstrap()
                .expect("valid bootstrap")
                .payload,
            payload
        );
        assert!(report.bootstrap_recovery_hints.is_none());
        for mismatch in ["identity", "block size", "parity scheme"] {
            let mut conflicting = hints.clone();
            match mismatch {
                "identity" => conflicting.tape_uuid = [0x99; 16],
                "block size" => conflicting.block_size *= 2,
                _ => conflicting.scheme = crate::ParityConfig::None,
            }
            // Validate geometry before scanning: fixed-size I/O can itself reject wrong sizes.
            assert!(conflicting
                .validate_bootstrap(&payload)
                .expect_err("conflict")
                .to_string()
                .contains(mismatch));
            let mut conflict_records = records.clone();
            if let Record::Block(block) = &mut conflict_records[0] {
                block.resize(conflicting.block_size as usize, 0);
            }
            let error = scan_reconstruct_filemark_map_with_report_mode(
                &mut RecordingRawSource::new(conflict_records),
                &conflicting.tape_uuid,
                conflicting.block_size,
                ScanMode::Recovery(&conflicting),
            )
            .expect_err("readable bootstrap conflict");
            assert!(error.to_string().contains(mismatch), "{error}");
        }
        for fault in [
            TestReadFault::Transport,
            TestReadFault::Hardware,
            TestReadFault::DeferredFixedMedium,
            TestReadFault::DeferredDescriptorMedium,
        ] {
            let error = scan_reconstruct_filemark_map_with_report_mode(
                &mut RecordingRawSource::new(vec![Record::ReadFault(fault), Record::Filemark]),
                &TAPE_UUID,
                BLOCK_SIZE,
                ScanMode::Recovery(&hints),
            )
            .expect_err("only current medium damage permits physical fallback");
            assert!(matches!(error, ParityError::TapeIo(_)));
        }
    }
    /// Recovery must distinguish validated refusals and header evidence from damaged payloads.
    #[test]
    fn scanner_recovery_preserves_compression_and_header_refusals() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("map");
        let payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        let scheme = payload.scheme.as_ref().expect("parity");
        let hints = ScanRecoveryHints {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            scheme: crate::ParityConfig::Scheme(ParityScheme {
                id: SchemeId::new_owned(scheme.id.clone()),
                data_blocks_per_stripe: scheme.data_blocks_per_stripe,
                parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
                stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            }),
        };
        let scan = |block, hints: &ScanRecoveryHints| {
            scan_reconstruct_filemark_map_with_report_mode(
                &mut RecordingRawSource::new(vec![Record::Block(block), Record::Filemark]),
                &hints.tape_uuid,
                hints.block_size,
                ScanMode::Recovery(hints),
            )
        };
        let mut compressed = bootstrap_block_for_payload(&payload);
        let end = 56 + u32::from_le_bytes(compressed[44..48].try_into().unwrap()) as usize;
        assert_eq!(&compressed[end - 2..end], &[5, 0xf4]);
        compressed[end - 1] = 0xf5;
        let crc = crate::crc64_xz(&compressed[56..end]);
        compressed[end..end + 8].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            scan(compressed, &hints),
            Err(ParityError::DriveCompressionEnabled)
        ));
        for field in ["identity", "block size"] {
            for failure in ["payload", "schema"] {
                let mut block = bootstrap_block_for_payload(&payload);
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
                let crc = crate::crc64_xz(&block[..48]);
                block[48..56].copy_from_slice(&crc.to_le_bytes());
                let error = scan(block.clone(), &hints).expect_err("checksummed header binds");
                let reason = if failure == "schema" {
                    "got 99, accept 2"
                } else {
                    field
                };
                assert!(error.to_string().contains(reason), "{error}");
                block[48] ^= 1;
                assert!(scan(block, &hints)
                    .expect("bad header has no identity evidence")
                    .bootstrap_recovery_hints
                    .is_some());
            }
        }
        for field in ["k", "m", "S"] {
            let mut conflicting = hints.clone();
            let crate::ParityConfig::Scheme(scheme) = &mut conflicting.scheme else {
                panic!("scheme")
            };
            match field {
                "k" => scheme.data_blocks_per_stripe -= 1,
                "m" => scheme.parity_blocks_per_stripe += 1,
                _ => scheme.stripes_per_neighborhood += 1,
            }
            let error = scan(bootstrap_block_for_payload(&payload), &conflicting)
                .expect_err("distinct parity geometry refuses");
            assert!(error.to_string().contains("parity scheme"), "{error}");
        }
    }

    /// Nonconformance cannot conceal authenticated scheme or compression conflicts.
    #[test]
    fn recovery_nonconformant_payload_preserves_readable_conflicts() {
        use ciborium::value::Value;
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("map");
        let payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        let scheme = payload.scheme.as_ref().expect("scheme");
        let hints = ScanRecoveryHints {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            scheme: crate::ParityConfig::Scheme(ParityScheme {
                id: SchemeId::new_owned(scheme.id.clone()),
                data_blocks_per_stripe: scheme.data_blocks_per_stripe,
                parity_blocks_per_stripe: scheme.parity_blocks_per_stripe,
                stripes_per_neighborhood: scheme.stripes_per_neighborhood,
            }),
        };
        for defect in ["key20", "key21", "key30", "digest", "noncanonical"] {
            for conflict in ["agree", "scheme", "none", "compression"] {
                let mut block = bootstrap_block_for_payload(&payload);
                let end = 56 + u32::from_le_bytes(block[44..48].try_into().unwrap()) as usize;
                let Value::Map(mut entries) =
                    ciborium::from_reader::<Value, _>(&block[56..end]).expect("decode payload")
                else {
                    panic!("map")
                };
                if conflict == "compression" {
                    entries
                        .iter_mut()
                        .find(|(k, _)| *k == Value::Integer(5.into()))
                        .expect("compression key")
                        .1 = Value::Bool(true);
                }
                match defect {
                    "key20" | "key21" | "key30" => {
                        let key: u8 = defect[3..].parse().expect("key");
                        entries.push((Value::Integer(key.into()), Value::Null));
                    }
                    "digest" => {
                        entries
                            .iter_mut()
                            .find(|(k, _)| *k == Value::Integer(2.into()))
                            .expect("digest key")
                            .1 = Value::Null;
                    }
                    _ => entries.swap(0, 1),
                }
                let mut bytes = Vec::new();
                ciborium::into_writer(&Value::Map(entries), &mut bytes).expect("encode payload");
                let end = 56 + bytes.len();
                block[44..48].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
                block[56..end].copy_from_slice(&bytes);
                block[end..end + 8].copy_from_slice(&crate::crc64_xz(&bytes).to_le_bytes());
                let mut supplied = hints.clone();
                if conflict == "scheme" {
                    let crate::ParityConfig::Scheme(scheme) = &mut supplied.scheme else {
                        panic!("scheme")
                    };
                    scheme.data_blocks_per_stripe -= 1;
                } else if conflict == "none" {
                    supplied.scheme = crate::ParityConfig::None;
                    block[12..16].copy_from_slice(&crate::bootstrap::FLAG_NO_PARITY.to_be_bytes());
                }
                let crc = crate::crc64_xz(&block[..48]);
                block[48..56].copy_from_slice(&crc.to_le_bytes());
                assert!(matches!(
                    parse_bootstrap_block(&block),
                    Err(ParityError::BootstrapParse(_))
                ));
                let result = supplied.classify_bootstrap(&block);
                match conflict {
                    "agree" => {
                        let RecoveryBootstrap::Unreadable(reason) =
                            result.expect("lenient recovery")
                        else {
                            panic!("nonconformant payload must remain unreadable")
                        };
                        assert!(reason.contains("readable but nonconformant"), "{reason}");
                    }
                    "compression" => {
                        assert!(matches!(result, Err(ParityError::DriveCompressionEnabled)))
                    }
                    _ => assert!(result
                        .expect_err("scheme conflict")
                        .to_string()
                        .contains("parity scheme")),
                }
                // Without the payload CRC, no decoded conflict is trustworthy.
                block[end] ^= 1;
                assert!(matches!(
                    supplied.classify_bootstrap(&block),
                    Ok(RecoveryBootstrap::Unreadable(_))
                ));
            }
        }
        let mut block = bootstrap_block_for_payload(&payload);
        block[44..48].copy_from_slice(&u32::MAX.to_le_bytes());
        let crc = crate::crc64_xz(&block[..48]);
        block[48..56].copy_from_slice(&crc.to_le_bytes());
        let RecoveryBootstrap::Unreadable(reason) =
            hints.classify_bootstrap(&block).expect("lenient bounds")
        else {
            panic!("payload past block cannot validate")
        };
        assert!(reason.contains("readable but nonconformant"), "{reason}");
    }

    /// Checksummed format and scheme refusals survive even a damaged payload.
    #[test]
    fn scanner_recovery_refuses_checked_format_fields() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("map");
        let mut payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        payload.no_parity_flag = true;
        payload.scheme = None;
        let hints = ScanRecoveryHints {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            scheme: crate::ParityConfig::None,
        };
        for (offset, bytes, reason) in [
            (8, 1u16.to_be_bytes().to_vec(), "got 1, accept 2"),
            (8, 3u16.to_be_bytes().to_vec(), "got 3, accept 2"),
            (36, 7u64.to_be_bytes().to_vec(), "got sequence 7"),
            (12, 0u32.to_be_bytes().to_vec(), "no-parity flag"),
        ] {
            for damaged_payload in [false, true] {
                let mut block = bootstrap_block_for_payload(&payload);
                block[offset..offset + bytes.len()].copy_from_slice(&bytes);
                if damaged_payload {
                    block[80] ^= 1;
                }
                let crc = crate::crc64_xz(&block[..48]);
                block[48..56].copy_from_slice(&crc.to_le_bytes());
                let error = scan_reconstruct_filemark_map_with_report_mode(
                    &mut RecordingRawSource::new(vec![
                        Record::Block(block.clone()),
                        Record::Filemark,
                    ]),
                    &TAPE_UUID,
                    BLOCK_SIZE,
                    ScanMode::Recovery(&hints),
                )
                .expect_err("checked header refusal");
                assert!(error.to_string().contains(reason), "{error}");
                block[48] ^= 1;
                assert!(matches!(
                    hints.classify_bootstrap(&block),
                    Ok(RecoveryBootstrap::Unreadable(_))
                ));
            }
        }
        let block = bootstrap_block_for_payload(&payload);
        for len in [block.len() - 1, block.len() + 1] {
            let mut resized = block.clone();
            resized.resize(len, 0);
            let error = hints
                .classify_bootstrap(&resized)
                .expect_err("block size refusal");
            assert!(error.to_string().contains("block size"), "{error}");
        }
        // Check the other direction of the flag disagreement as well.
        let parity_hints = ScanRecoveryHints {
            scheme: crate::ParityConfig::Scheme(crate::default_scheme_for_block_size(BLOCK_SIZE)),
            ..hints
        };
        assert!(parity_hints
            .classify_bootstrap(&block)
            .expect_err("no-parity conflicts with parity hints")
            .to_string()
            .contains("no-parity flag"));
    }

    /// Direct BOT recovery preserves the typed identity refusal in both modes.
    #[test]
    fn bot_recovery_preserves_typed_foreign_identity_refusal() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("map");
        let mut payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        payload.tape_uuid = [0x99; 16];
        let hints = ScanRecoveryHints {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            scheme: crate::ParityConfig::None,
        };
        for mode in [ScanMode::Standard, ScanMode::Recovery(&hints)] {
            let error = crate::bot_recovery::recover_terminal_inventory_from_bot_controlled_mode(
                &mut RecordingRawSource::new(vec![
                    Record::Block(bootstrap_block_for_payload(&payload)),
                    Record::Filemark,
                ]),
                &TAPE_UUID,
                BLOCK_SIZE,
                mode,
                |_| ScanWalkControl::Continue,
                |_| Ok(()),
            )
            .expect_err("foreign BOT");
            assert!(
                matches!(
                    error,
                    crate::BotStructuralRecoveryError::TapeIdentityMismatch
                ),
                "{error}"
            );
        }
    }

    /// Recovery retains the one-block BOT rule and validates even a torn valid BOT.
    #[test]
    fn scanner_recovery_preserves_bot_shape_and_torn_head_identity() {
        let hints = ScanRecoveryHints {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            scheme: crate::ParityConfig::None,
        };
        let error = scan_reconstruct_filemark_map_with_report_mode(
            &mut RecordingRawSource::new(vec![
                Record::Block(block(0)),
                Record::Block(block(0)),
                Record::Filemark,
            ]),
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Recovery(&hints),
        )
        .expect_err("recovery cannot invent a one-block BOT out of two blocks");
        assert!(error.to_string().contains("requires one Bootstrap block"));
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("BOT map");
        let mut payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        payload.tape_uuid = [0x99; 16];
        let error = scan_reconstruct_filemark_map_with_report_mode(
            &mut RecordingRawSource::new(vec![Record::Block(bootstrap_block_for_payload(
                &payload,
            ))]),
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Recovery(&hints),
        )
        .expect_err("a missing trailing filemark cannot hide a readable UUID conflict");
        assert!(error.to_string().contains("identity mismatch"));
        let error = scan_reconstruct_filemark_map_with_report_mode(
            &mut RecordingRawSource::new(vec![Record::Block(block(0))]),
            &TAPE_UUID,
            BLOCK_SIZE,
            ScanMode::Recovery(&hints),
        )
        .expect_err("recovery still requires a complete structural BOT file");
        assert!(error.to_string().contains("sole tape-file-0 BOT Bootstrap"));
    }
}
