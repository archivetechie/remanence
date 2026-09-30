//! Catalog-less filemark-map reconstruction for Layer 3c v0.4.4.
//!
//! The scanner walks physical tape files from BOT, reads only the first block
//! of each file and eligible footer/tail fallbacks for classification, and measures file length by
//! spacing to the next filemark. Bootstrap, parity-map, and sidecar tape files
//! are classified on the first pass after magic plus CRC/header validation.
//! A final directory can identify sidecars by file number and measured length;
//! recovery authority then requires validation of the whole prefix projection.
//! Terminal replica/separation magic is structurally reserved: damaged terminal
//! framing remains typed control evidence so it cannot consume Object ordinals.

use crate::bootstrap::{has_bootstrap_magic, parse_bootstrap_block, BootstrapPayload};
use crate::error::{BootstrapRefusedField, ParityError};
use crate::filemark_map::{
    FilemarkMap, FilemarkMapBuilder, ScopedFilemarkMap, TapeFileKind, TapeFileMapEntry,
    TapeFilePosition,
};
use crate::index_separation::{
    derive_index_separation_footer_magic, derive_index_separation_header_magic,
    parse_index_separation_footer, parse_index_separation_header,
};
#[cfg(test)]
use crate::parity_map::read_final_sidecar_directory;
use crate::parity_map::{
    classify_parity_map_header_block, parse_parity_map_footer_block, parse_parity_map_header_block,
    read_final_parity_map, validate_header_matches_footer, DecodedParityMapTapeFile,
};
use crate::raw::{
    classify_fixed_record, device_position_error, read_fixed_record,
    tape_error_is_current_medium_damage, wrong_record_length, FixedRecordRead,
    PhysicalPositionHint, RawReadOutcome, RawTapeSource,
};
#[cfg(test)]
use crate::recovery::read_directory_tail_index;
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
    /// Refuse a first record whose measured length differs from the supplied
    /// block size (REM-PARITY 8.4). Every hinted path compares the length here.
    fn refuse_record_length(
        &self,
        measured: usize,
        detail: impl FnOnce() -> String,
    ) -> Result<(), ParityError> {
        if usize::try_from(self.block_size).ok() == Some(measured) {
            Ok(())
        } else {
            Err(bootstrap_refused(
                BootstrapRefusedField::BlockSize,
                detail(),
            ))
        }
    }

    /// Judge the measured length of a first-record read that did not fill its
    /// read size exactly: a short read's `bytes`, or the `actual` of
    /// [`remanence_library::TapeIoError::ReadBufferTooSmall`]. The comparison is always with the
    /// supplied block size (REM-PARITY 8.4), so at a candidate read size other
    /// than the supplied one, a record of the supplied length passes and only
    /// that candidate is ruled out by the caller. A read that fills its read
    /// size is judged by [`Self::classify_bootstrap`], which checks the length
    /// first. Every other outcome is left to the caller.
    pub fn check_bootstrap_read_length(
        &self,
        read: &Result<RawReadOutcome, ParityError>,
        read_size: usize,
    ) -> Result<(), ParityError> {
        // The raw layer's one length judgement measures the record; the
        // comparison here is with the supplied size.
        let Some(measured) = wrong_record_length(read, read_size) else {
            return Ok(());
        };
        let measured_usize = usize::try_from(measured).unwrap_or(usize::MAX);
        self.refuse_record_length(measured_usize, || {
            if measured_usize < read_size {
                format!(
                    "short fixed-block bootstrap read: got {measured} bytes, supplied block size is {}",
                    self.block_size
                )
            } else {
                format!(
                    "bootstrap block larger than supplied block size: got {measured} bytes, expected {}",
                    self.block_size
                )
            }
        })
    }

    /// Classify a recovery BOT block, refusing validated disagreements before
    /// parsing its payload (REM-PARITY 8.4). The record's length is judged
    /// first; the physical read outcome is handled by the caller through
    /// [`Self::check_bootstrap_read_length`].
    pub fn classify_bootstrap(&self, block: &[u8]) -> Result<RecoveryBootstrap, ParityError> {
        self.refuse_record_length(block.len(), || {
            format!(
                "readable bootstrap block size differs from supplied hints: got {} bytes, expected {}",
                block.len(),
                self.block_size
            )
        })?;
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
            return Err(bootstrap_refused(
                BootstrapRefusedField::FormatMajor,
                format!(
                    "unsupported bootstrap schema major version: got {major}, accept {}",
                    crate::bootstrap::BOOTSTRAP_SCHEMA_MAJOR
                ),
            ));
        }
        if block[16..32] != self.tape_uuid {
            return Err(bootstrap_refused(
                BootstrapRefusedField::TapeUuid,
                "tape identity mismatch: readable bootstrap header differs from supplied hints",
            ));
        }
        if u32::from_be_bytes(block[32..36].try_into().expect("header block size bytes"))
            != self.block_size
        {
            return Err(bootstrap_refused(
                BootstrapRefusedField::BlockSize,
                "readable bootstrap block size differs from supplied hints",
            ));
        }
        let sequence = u64::from_be_bytes(block[36..44].try_into().expect("header sequence bytes"));
        if sequence != 0 {
            return Err(bootstrap_refused(
                BootstrapRefusedField::Sequence,
                format!(
                    "schema-major 2 permits only the sequence-0 BOT Bootstrap: got sequence {sequence}"
                ),
            ));
        }
        let flags = u32::from_be_bytes(block[12..16].try_into().expect("header flags bytes"));
        let no_parity = flags & crate::bootstrap::FLAG_NO_PARITY != 0;
        if no_parity != matches!(self.scheme, crate::ParityConfig::None) {
            return Err(bootstrap_refused(
                BootstrapRefusedField::NoParityFlag,
                "readable bootstrap parity scheme differs from supplied hints: no-parity flag contradicts scheme",
            ));
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
                    // Recorded compression is judged before the scheme, as the
                    // parser does, and only on a parity tape (REM-PARITY 16.3).
                    if !no_parity
                        && entries.iter().any(|(key, value)| {
                            *key == Value::Integer(5.into()) && *value == Value::Bool(true)
                        })
                    {
                        return Err(ParityError::DriveCompressionEnabled {
                            context: crate::error::CompressionRefusalContext::Bootstrap,
                        });
                    }
                    for (key, value) in &entries {
                        if *key != Value::Integer(1.into()) {
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
                                return Err(bootstrap_refused(
                                    BootstrapRefusedField::Scheme,
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
        crate::bootstrap::no_parity_scheme_conflict(
            bootstrap.no_parity_flag,
            bootstrap.scheme.is_some(),
        )
        .map_err(|message| bootstrap_refused(BootstrapRefusedField::Scheme, message))?;
        if self.tape_uuid != bootstrap.tape_uuid {
            return Err(bootstrap_refused(
                BootstrapRefusedField::TapeUuid,
                "tape identity mismatch: readable bootstrap differs from supplied hints",
            ));
        }
        if self.block_size != bootstrap.block_size_bytes {
            return Err(bootstrap_refused(
                BootstrapRefusedField::BlockSize,
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
            return Err(bootstrap_refused(
                BootstrapRefusedField::Scheme,
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
    /// Final ParityMap read once for both reconciliation and scope validation.
    final_parity_map: Option<Box<DecodedParityMapTapeFile>>,
    /// Structurally complete files walked before EOD or truncation.
    pub map: FilemarkMap,
    /// First incomplete tail file, when one terminated the walk.
    pub truncation: Option<ScanTailTruncation>,
    /// Best structural classification of the torn tail file from its readable
    /// first block. Recognisable terminal control magic is always preserved as
    /// control evidence and never falls through to Object.
    pub truncation_candidate_kind: Option<TapeFileKind>,
    /// The valid BOT bootstrap, if encountered and structurally classified by
    /// the walk. REM-PARITY §8.3: "Exactly one bootstrap is mandatory at BOT,
    /// with sequence 0. No intermediate or final bootstrap is permitted."
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

/// The valid BOT bootstrap encountered during the structural walk.
/// REM-PARITY §8.3: "Exactly one bootstrap is mandatory at BOT, with sequence 0."
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScanBootstrapCandidate {
    /// Dense tape-file number containing the bootstrap.
    pub tape_file_number: u64,
    /// Fully parsed bootstrap payload.
    pub payload: BootstrapPayload,
    /// Nonzero trailing fill: accepted by Readers, reported only by Verifiers.
    pub nonzero_fill: bool,
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
    /// A tape file whose last block parsed as a sidecar footer with the
    /// measured count carried a tail header copy that read but did not parse,
    /// or that parsed but disagreed with the footer's locator in some field, so the file is not recognised
    /// as a sidecar (REM-PARITY 12.3 item 6) and fell through to the next
    /// rung. The walk continues; the failure is reported.
    ClassificationTailMismatch,
    /// A terminal-control magic was present but its frame or measured count
    /// was invalid. It remains a control file and never consumes Object
    /// ordinals or participates in Object-based overlays.
    InvalidTerminalControl,
    /// The first record of a tape file after BOT was shorter or longer than
    /// one block (REM-PARITY 3.5). It is invalid content, not a device failure:
    /// the file is classified as for an unreadable head.
    WrongLengthTapeFileHead,
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

impl ScanWalkResult {
    /// The final ParityMap the walk read, when it validates and is marked
    /// `is_final_directory` (REM-PARITY 13.1). It scopes a walked map without
    /// any bootstrap.
    pub(crate) fn final_parity_map(&self) -> Option<&DecodedParityMapTapeFile> {
        self.final_parity_map.as_deref()
    }
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
/// validates the reconciled map against the final ParityMap, or retains only
/// the bootstrap's prefix scope when no validated final ParityMap is available.
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
        return Err(ParityError::DriveCompressionEnabled {
            context: crate::error::CompressionRefusalContext::Bootstrap,
        });
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

/// Validate one reconciled structural scan against the final ParityMap scope,
/// falling back to the bootstrap scope only when no valid final ParityMap exists.
///
/// Catalog-less report consumers call the structural scan once, select the
/// authoritative bootstrap from its candidates, and pass that same walk here.
/// This reuses the final ParityMap loaded by the walk without further tape I/O.
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
    let scoped_map = match reconstructed.final_parity_map {
        Some(parity_map) => {
            if parity_map.header.tape_uuid != authoritative_bootstrap.tape_uuid
                || parity_map.header.block_size != authoritative_bootstrap.block_size_bytes
            {
                return Err(filemark_scan_error(
                    "walked ParityMap disagrees with bootstrap identity or block size",
                ));
            }
            ScopedFilemarkMap::validate_against_final_parity_map(reconstructed.map, &parity_map)
        }
        None => ScopedFilemarkMap::validate_against_digest(reconstructed.map, digest),
    };
    match scoped_map {
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
/// required to derive the HMAC sidecar magic. Before recovery the caller must
/// validate the reconciled map against the final ParityMap, or retain only the
/// bootstrap scope; see [`validate_scan_reconstruction_with_report`]. If scanning
/// completes but that digest check fails, one possible cause is that the caller used a
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
    scan_with_bootstrap_observer(
        source,
        tape_uuid,
        block_size,
        mode,
        &mut control,
        &mut |_| {},
    )
}

/// Expose bootstrap evidence as it is classified, including when a later read fails.
pub(crate) fn scan_with_bootstrap_observer<F>(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    mode: ScanMode<'_>,
    control: &mut F,
    bootstrap_observer: &mut dyn FnMut(&ScanBootstrapCandidate),
) -> Result<ControlledScanWalkOutcome, ParityError>
where
    F: FnMut(&ScanWalkProgress) -> ScanWalkControl,
{
    match scan_reconstruct_filemark_map_with_provenance(
        source,
        tape_uuid,
        block_size,
        mode,
        control,
        bootstrap_observer,
    )? {
        ScanReconstructionOutcome::Complete(mut reconstructed) => {
            let final_parity_map =
                reconcile_walk_sidecars(source, &mut reconstructed.map, tape_uuid, block_size)?;
            Ok(ControlledScanWalkOutcome::Complete(ScanWalkResult {
                final_parity_map: final_parity_map.map(Box::new),
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

/// Revisit only the final directory's prefix after the physical BOT walk.
/// Identify Object candidates by directory tape file and measured length without
/// reading sidecar metadata. Keep identifications only after the final ParityMap
/// confirms the whole projection and scope; otherwise preserve the first pass.
/// Retain the decoded ParityMap for recovery validation without another read.
fn reconcile_walk_sidecars(
    source: &mut dyn RawTapeSource,
    map: &mut FilemarkMap,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<Option<DecodedParityMapTapeFile>, ParityError> {
    let Some(parity_map) = read_final_parity_map(source, map, tape_uuid, block_size)? else {
        return Ok(None);
    };
    let directory = &parity_map.payload.directory;
    if !map
        .entries()
        .iter()
        .rev()
        .find(|entry| entry.kind == TapeFileKind::ParityMap)
        .is_some_and(|entry| {
            entry.tape_file_number.checked_add(1) == Some(directory.directory_scope_tape_file_count)
        })
    {
        return Ok(Some(parity_map));
    }
    let mut entries = map.entries().to_vec();
    for entry in &directory.entries {
        // The parser checks each entry against this directory's own prefix.
        let measured = &entries[entry.tape_file_number as usize];
        // Preserve every walk-classified sidecar without I/O; recovery checks
        // directory agreement before placing any directory-assisted read.
        if measured.kind != TapeFileKind::Object {
            continue;
        }
        if measured.block_count != entry.sidecar_total_block_count {
            continue;
        }
        entries[entry.tape_file_number as usize] = TapeFileMapEntry::parity_sidecar(
            entry.tape_file_number,
            measured.block_count,
            entry.epoch_id,
            entry.protected_ordinal_start,
            entry.protected_ordinal_end_exclusive,
        );
    }
    let mut ordinal = 0u64;
    for entry in &mut entries {
        if entry.kind == TapeFileKind::Object {
            entry.first_parity_data_ordinal = Some(ordinal);
            ordinal = ordinal
                .checked_add(entry.block_count)
                .ok_or_else(|| filemark_scan_error("reconciled Object ordinals overflow"))?;
        }
    }
    let reconciled = FilemarkMap::new(entries)?;
    match ScopedFilemarkMap::validate_against_final_parity_map(reconciled, &parity_map) {
        Ok(confirmed) => *map = confirmed.map,
        Err(ParityError::FilemarkMapDigestMismatch { .. }) => {}
        Err(error) => return Err(error),
    }
    Ok(Some(parity_map))
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
    bootstrap_observer: &mut dyn FnMut(&ScanBootstrapCandidate),
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
        let at_bot = builder.next_tape_file_number()? == 0 && file_start.lba == 0;
        // Supplied values judge only tape file 0's record at LBA 0 (REM-PARITY 8.4).
        let bot_hints = match mode {
            ScanMode::Recovery(hints) if at_bot => Some(hints),
            _ => None,
        };
        let read = source.read_record(&mut buf);
        if let Some(hints) = bot_hints {
            // The measured length is judged before anything else.
            hints.check_bootstrap_read_length(&read, block_size_usize)?;
        }
        // REM-PARITY 3.5: a head record shorter or longer than one block is a
        // fact about the tape, never TapeIo.
        let head = match classify_fixed_record(read, block_size_usize) {
            Ok(FixedRecordRead::EndOfData { .. }) => ScanHeadRead::EndOfData,
            Ok(FixedRecordRead::Filemark { .. }) => ScanHeadRead::Filemark,
            Ok(FixedRecordRead::Block { .. }) => ScanHeadRead::Block,
            Ok(FixedRecordRead::WrongLength { measured_bytes }) if at_bot => {
                // The first record with a known block size (Sections 3.5, 8.4
                // and 15): a record of another length is `BootstrapParse`.
                return Err(ParityError::BootstrapParse(format!(
                    "first record is {measured_bytes} bytes, not the known block size {block_size}"
                )));
            }
            // Elsewhere it is an invalid candidate for every control rung that
            // reads the head: the file is classified as for an unreadable head,
            // by filemark spacing, the footer probes, and otherwise as an Object
            // candidate. Its bytes are never read as a head.
            Ok(FixedRecordRead::WrongLength { .. }) => {
                ScanHeadRead::Invalid(ScanDamageKind::WrongLengthTapeFileHead)
            }
            Err(error) if scan_read_error_is_medium_damage(&error) => {
                ScanHeadRead::Invalid(ScanDamageKind::UnreadableTapeFileHead)
            }
            Err(error) => return Err(error),
        };
        match head {
            // A filemark or EOD where the first record should be is an unreadable
            // bootstrap, never a refusal (REM-PARITY 8.4): the walk continues on
            // the supplied values, and ends here with no tape file 0 to map.
            ScanHeadRead::EndOfData => break,
            ScanHeadRead::Filemark => {
                truncation = Some(ScanTailTruncation {
                    tape_file_number: builder.next_tape_file_number()?,
                    position: file_start,
                    kind: ScanTailTruncationKind::EmptyFile,
                });
                break;
            }
            ScanHeadRead::Block => {
                let first_block = buf.clone();
                let mut invalid_bootstrap = false;
                if let Some(hints) = bot_hints {
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
                let measured = match measure_current_file(source, file_start)? {
                    MeasureCurrentFileOutcome::Complete(measured) => measured,
                    MeasureCurrentFileOutcome::Truncated(kind) => {
                        truncation_candidate_kind = Some(classify_truncated_file_head(
                            &first_block,
                            tape_uuid,
                            block_size,
                            at_bot,
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
                    bootstrap_observer(&candidate);
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
            ScanHeadRead::Invalid(kind) => {
                damaged_regions.push(ScanDamagedRegion {
                    start: file_start,
                    block_count: 1,
                    kind,
                });
                if let Some(hints) = bot_hints {
                    bootstrap_recovery_hints = Some(hints.clone());
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

/// The walk's head read of one tape file, after REM-PARITY 3.5's length rule.
enum ScanHeadRead {
    EndOfData,
    Filemark,
    /// One block; its bytes are in the walk's buffer.
    Block,
    /// Unreadable, or a record of the wrong length: no head bytes to classify.
    Invalid(ScanDamageKind),
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

    // Both positions are device reports (REM-PARITY 12.2 measures the file by
    // filemark spacing), so a delta that goes backwards, or a count that does
    // not fit, faults the device: `TapeIo`, never a wrapped count and never a
    // zero-block file (Section 2.4).
    let consumed = outcome
        .position_after
        .lba
        .checked_sub(file_start.lba)
        .ok_or_else(|| {
            device_position_error(format_args!(
                "went backwards: spacing from LBA {} over one filemark reported LBA {}",
                file_start.lba, outcome.position_after.lba
            ))
        })?;
    let block_count = consumed.checked_sub(1).ok_or_else(|| {
        device_position_error(format_args!(
            "did not advance past the filemark: spacing from LBA {} reported LBA {}, so the block count is -1",
            file_start.lba, outcome.position_after.lba
        ))
    })?;
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
        match crate::bootstrap::parse_bootstrap_block_with_fill(block0) {
            Ok((payload, nonzero_fill)) => {
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
                        nonzero_fill,
                    }));
                }
            }
            Err(error @ ParityError::DriveCompressionEnabled { .. }) => {
                return Err(error);
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

    // §12.3 shares this last-block read between items 2/3, 4 and 6,
    // including an unreadable result: do not retry a medium error per probe.
    let footer_block =
        read_optional_fixed_block_at(source, file_start, block_count - 1, block_size)?;
    if builder.next_tape_file_number()? != 0
        && classify_parity_map_from_footer_tail(
            footer_block.as_deref(),
            source,
            file_start,
            tape_uuid,
            block_size,
            block_count,
            damaged_regions,
        )?
    {
        builder.push_parity_map(block_count)?;
        return Ok(None);
    }

    if let Some(kind) = classify_terminal_from_footer_tail(
        footer_block.as_deref(),
        file_start,
        tape_uuid,
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
        footer_block.as_deref(),
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
    // §12.3 shares this last-block read between items 2/3, 4 and 6,
    // including an unreadable result: do not retry a medium error per probe.
    let footer_block =
        read_optional_fixed_block_at(source, file_start, block_count - 1, block_size)?;
    if builder.next_tape_file_number()? != 0
        && classify_parity_map_from_footer_tail(
            footer_block.as_deref(),
            source,
            file_start,
            tape_uuid,
            block_size,
            block_count,
            damaged_regions,
        )?
    {
        builder.push_parity_map(block_count)?;
        return Ok(false);
    }

    if let Some(kind) = classify_terminal_from_footer_tail(
        footer_block.as_deref(),
        file_start,
        tape_uuid,
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
        footer_block.as_deref(),
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

/// Item 4's fallback reads only the footer and the located tail header. Count
/// disagreement is damage even when no tail header can be parsed.
#[allow(clippy::too_many_arguments)]
fn classify_parity_map_from_footer_tail(
    footer_block: Option<&[u8]>,
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
    tape_uuid: &[u8; 16],
    block_size: u32,
    block_count: u64,
    damaged_regions: &mut Vec<ScanDamagedRegion>,
) -> Result<bool, ParityError> {
    let Some(block) = footer_block else {
        return Ok(false);
    };
    let Ok(footer) = parse_parity_map_footer_block(block, tape_uuid) else {
        return Ok(false);
    };
    if footer.parity_map_total_block_count != block_count {
        damaged_regions.push(ScanDamagedRegion {
            start: file_start,
            block_count,
            kind: ScanDamageKind::ClassificationCountMismatch,
        });
        return Ok(false);
    }
    let Some(block) =
        read_optional_fixed_block_at(source, file_start, footer.tail_copy_start_block, block_size)?
    else {
        return Ok(false);
    };
    Ok(
        parse_parity_map_header_block(&block, tape_uuid).is_ok_and(|header| {
            // This route locates the tail copy; the primary route accepts either kind.
            header.copy_kind == crate::ParityMapCopyKind::Tail
                && validate_header_matches_footer(&header, &footer).is_ok()
        }),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalControlScanClassification {
    Replica,
    Separation,
}

fn classify_terminal_from_footer_tail(
    footer_block: Option<&[u8]>,
    file_start: PhysicalPositionHint,
    tape_uuid: &[u8; 16],
    block_count: u64,
    damaged_regions: &mut Vec<ScanDamagedRegion>,
) -> Result<Option<TerminalControlScanClassification>, ParityError> {
    let Some(footer_block) = footer_block else {
        return Ok(None);
    };
    let magic = footer_block.get(..8);
    let replica_magic = derive_tape_index_replica_footer_magic(tape_uuid);
    if magic.is_some_and(|prefix| prefix == replica_magic) {
        let valid_count = parse_tape_index_bootstrap_footer(footer_block, tape_uuid)
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
        let valid_count = parse_index_separation_footer(footer_block, tape_uuid)
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

#[allow(clippy::too_many_arguments)]
fn classify_sidecar_from_footer_tail(
    footer_block: Option<&[u8]>,
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
    tape_uuid: &[u8; 16],
    block_size: u32,
    block_count: u64,
    damaged_regions: &mut Vec<ScanDamagedRegion>,
) -> Result<Option<SidecarScanClassification>, ParityError> {
    let Some(footer_block) = footer_block else {
        return Ok(None);
    };
    let footer = match parse_sidecar_footer_block(footer_block, tape_uuid) {
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
        SidecarTailProbe::Matches(header) => Ok(Some(SidecarScanClassification::from(&header))),
        // REM-PARITY 12.3 item 6: the Scanner MAY classify from the footer
        // fields alone if the tail copy is unreadable.
        SidecarTailProbe::Unreadable => Ok(Some(SidecarScanClassification::from(&footer))),
        // Item 6: a tail copy that disagrees with the footer, field for field,
        // means the file is not recognised. Report it and fall through to the
        // next rung, as the count mismatch above does, rather than abandoning
        // the whole walk over one tape file's disagreement.
        SidecarTailProbe::Disagrees => {
            damaged_regions.push(ScanDamagedRegion {
                start: file_start,
                block_count,
                kind: ScanDamageKind::ClassificationTailMismatch,
            });
            Ok(None)
        }
    }
}

/// What the footer/tail probe of REM-PARITY 12.3 item 6 found in the tail copy.
enum SidecarTailProbe {
    /// A readable tail copy that agrees with the footer's locator.
    Matches(SidecarHeader),
    /// A block of the tail copy could not be read (or lies outside the file).
    Unreadable,
    /// A tail copy that reads but is invalid, or that parses and disagrees
    /// with the footer in some field.
    Disagrees,
}

fn read_tail_sidecar_header(
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
    tape_uuid: &[u8; 16],
    block_size: u32,
    footer: &SidecarFooter,
) -> Result<SidecarTailProbe, ParityError> {
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
            return Ok(SidecarTailProbe::Unreadable);
        };
        blocks.push(block);
    }
    // A tail copy that reads but fails to parse (a CRC or hash failure) is an
    // invalid copy, not an unreadable one: it cannot be verified against the
    // footer field for field, so the file is not recognised (12.3 item 6).
    let decoded = match parse_sidecar_index_blocks(&blocks, tape_uuid) {
        Ok(decoded) => decoded,
        Err(_) => return Ok(SidecarTailProbe::Disagrees),
    };
    if !sidecar_header_matches_footer(&decoded.header, footer) {
        return Ok(SidecarTailProbe::Disagrees);
    }
    Ok(SidecarTailProbe::Matches(decoded.header))
}

fn read_optional_fixed_block_at(
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
    block_within_file: u64,
    block_size: u32,
) -> Result<Option<Vec<u8>>, ParityError> {
    // Every offset passed here is below the file's measured block count (the
    // last block, or a tail copy that a footer whose total equals the measured
    // count places inside the file), so the sum lies before the device's
    // post-space position and cannot overflow. Were it to, the offset would be
    // a recorded value outside the file, a format fact rather than a device
    // report, so the distinction is kept and this stays a map error.
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
    // A probed record of the wrong length (REM-PARITY 3.5), like a boundary or
    // a medium error, is no candidate for the probed control structure.
    match read_fixed_record(source, &mut buf) {
        Ok(FixedRecordRead::Block { .. }) => Ok(Some(buf)),
        Ok(
            FixedRecordRead::WrongLength { .. }
            | FixedRecordRead::Filemark { .. }
            | FixedRecordRead::EndOfData { .. },
        ) => Ok(None),
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

fn bootstrap_refused(field: BootstrapRefusedField, detail: impl Into<String>) -> ParityError {
    ParityError::BootstrapRefused {
        field,
        detail: detail.into(),
    }
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
        locate_fault: Option<(u64, TestReadFault)>,
    }

    impl RecordingRawSource {
        fn new(records: Vec<Record>) -> Self {
            Self {
                records,
                cursor: 0,
                calls: Vec::new(),
                locate_fault: None,
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
            if let Some((lba, fault)) = self.locate_fault {
                if hint.lba == lba {
                    return Err(fault.error());
                }
            }
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

    /// Build fresh encoded metadata and a recording source, without disk fixtures.
    fn directory_scan_source() -> (RecordingRawSource, FilemarkMap, crate::ParityMapPayload) {
        use crate::sidecar::{encode_sidecar_tape_file, SidecarDescriptor};
        use crate::{
            encode_parity_map_tape_file, SidecarEpochDirectory, SidecarEpochDirectoryEntry,
        };
        let sidecar = encode_sidecar_tape_file(
            &SidecarDescriptor {
                tape_uuid: TAPE_UUID,
                epoch_id: 0,
                k: 2,
                m: 1,
                stripes_per_epoch: 1,
                block_size: BLOCK_SIZE,
                protected_ordinal_start: 0,
                protected_ordinal_end_exclusive: 2,
            },
            &[block(0x33)],
            vec![0, 0],
        )
        .unwrap();
        let header = &sidecar.header;
        let mut payload = crate::ParityMapPayload {
            tape_uuid: TAPE_UUID,
            sequence: 0,
            directory: SidecarEpochDirectory {
                directory_scope_tape_file_count: 4,
                directory_scope_total_data_ordinals: 2,
                directory_scope_highest_protected_ordinal: 2,
                is_final_directory: true,
                entries: vec![SidecarEpochDirectoryEntry {
                    tape_file_number: 2,
                    epoch_id: 0,
                    protected_ordinal_start: 0,
                    protected_ordinal_end_exclusive: 2,
                    sidecar_total_block_count: header.sidecar_total_block_count,
                    sidecar_header_block_count: header.shard_index_block_count,
                    parity_shard_block_count: header.parity_block_count,
                    canonical_metadata_hash: header.canonical_metadata_hash,
                    flags: 0,
                }],
            },
            canonical_map_digest: [0; 32],
            writer_version: None,
            write_timestamp: None,
        };
        let parity_map = encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar.blocks.len() as u64, 0, 0, 2),
            TapeFileMapEntry::parity_map(3, parity_map.blocks.len() as u64),
        ])
        .unwrap();
        payload.canonical_map_digest = map.canonical_digest().unwrap();
        let parity_map = encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        assert_eq!(parity_map.blocks.len() as u64, map.entries()[3].block_count);
        let bot_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).unwrap();
        let mut records = vec![
            Record::Block(bootstrap_block(bot_map.digest(false).unwrap(), 0)),
            Record::Filemark,
            Record::Block(block(0x11)),
            Record::Block(block(0x22)),
            Record::Filemark,
        ];
        records.extend(sidecar.blocks.into_iter().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(parity_map.blocks.into_iter().map(Record::Block));
        records.push(Record::Filemark);
        (RecordingRawSource::new(records), map, payload)
    }

    /// Exercise item 4 on a real encoded file, retaining the recording source
    /// so the tests also prove the fallback reads only the footer and header.
    fn parity_map_tail_case(
        head_offset: Option<usize>,
        bad_footer: bool,
        extra: bool,
        bad_tail: bool,
    ) -> (ScanWalkResult, RecordingRawSource, u64) {
        let (mut source, map, _) = directory_scan_source();
        let start = map
            .physical_position(TapeFilePosition {
                tape_file_number: 3,
                block_within_file: 0,
            })
            .unwrap()
            .lba;
        let end = source.records.len() - 2;
        if let Some(offset) = head_offset {
            let Record::Block(block) = &mut source.records[start as usize] else {
                panic!("header fixture")
            };
            block[offset] ^= 1;
        } else {
            source.records[start as usize] = Record::ReadFault(TestReadFault::Medium);
        }
        if bad_footer {
            let Record::Block(block) = &mut source.records[end] else {
                panic!("footer fixture")
            };
            block[0xc0] ^= 1;
        }
        if bad_tail {
            let Record::Block(block) = &mut source.records[start as usize + 1] else {
                panic!("tail fixture")
            };
            block[0xc0] ^= 1;
        }
        if extra {
            source.records.insert(end, Record::Block(block(0x55)));
        }
        // Use the first pass: directory reconciliation must not conceal which
        // rung classified the file or add payload reads to this observation.
        let ScanReconstructionOutcome::Complete(walked) =
            scan_reconstruct_filemark_map_with_provenance(
                &mut source,
                &TAPE_UUID,
                BLOCK_SIZE,
                ScanMode::Standard,
                &mut |_| ScanWalkControl::Continue,
                &mut |_| {},
            )
            .unwrap()
        else {
            panic!("walk aborted")
        };
        let report = ScanWalkResult {
            final_parity_map: None,
            map: walked.map,
            truncation: walked.truncation,
            truncation_candidate_kind: walked.truncation_candidate_kind,
            bootstrap_candidates: walked.bootstrap_candidates,
            damaged_regions: walked.damaged_regions,
            bootstrap_recovery_hints: walked.bootstrap_recovery_hints,
        };
        (report, source, start)
    }

    #[test]
    fn parity_map_tail_route_unreadable_head() {
        let (walked, source, start) = parity_map_tail_case(None, false, false, false);
        assert_eq!(walked.map.entries()[3].kind, TapeFileKind::ParityMap);
        let reads: Vec<_> = source
            .calls
            .iter()
            .filter_map(|c| match c {
                ScanCall::ReadRecord(lba) if *lba >= start => Some(*lba),
                _ => None,
            })
            .collect();
        assert_eq!(reads, vec![start, start + 2, start + 1, start + 4]);
    }

    #[test]
    fn parity_map_tail_route_damaged_magic() {
        let (walked, _, _) = parity_map_tail_case(Some(0), false, false, false);
        assert_eq!(walked.map.entries()[3].kind, TapeFileKind::ParityMap);
    }

    #[test]
    fn parity_map_tail_route_damaged_header_crc() {
        let (walked, _, _) = parity_map_tail_case(Some(0xc0), false, false, false);
        assert_eq!(walked.map.entries()[3].kind, TapeFileKind::ParityMap);
    }

    #[test]
    fn parity_map_tail_route_count_mismatch_even_with_invalid_tail() {
        for (head, bad_tail) in [None, Some(0xc0)]
            .into_iter()
            .flat_map(|head| [false, true].map(|tail| (head, tail)))
        {
            let (walked, _, start) = parity_map_tail_case(head, false, true, bad_tail);
            assert_eq!(walked.map.entries()[3].kind, TapeFileKind::Object);
            assert!(walked
                .damaged_regions
                .iter()
                .any(|d| d.start.lba == start
                    && d.kind == ScanDamageKind::ClassificationCountMismatch));
        }
    }

    #[test]
    fn parity_map_tail_route_unusable_footer() {
        for head in [None, Some(0xc0)] {
            let (walked, _, _) = parity_map_tail_case(head, true, false, false);
            assert_eq!(walked.map.entries()[3].kind, TapeFileKind::Object);
        }
    }

    #[test]
    fn parity_map_tail_route_never_at_tape_file_zero() {
        let (_, _, payload) = directory_scan_source();
        let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        for count in [1, encoded.blocks.len() as u64] {
            let mut source = RecordingRawSource::new(
                encoded.blocks.iter().cloned().map(Record::Block).collect(),
            );
            let result = append_entry_with_unreadable_head(
                &mut source,
                &mut FilemarkMapBuilder::new(),
                &TAPE_UUID,
                BLOCK_SIZE,
                PhysicalPositionHint::new(0),
                count,
                &mut Vec::new(),
            );
            if count == 1 {
                assert!(!result.unwrap());
            } else {
                assert!(matches!(
                    result,
                    Err(ParityError::FilemarkMapReconstruct(_))
                ));
            }
            assert!(
                source.calls.is_empty(),
                "BOT must not probe any footer or tail"
            );
        }
    }

    /// A readable but invalid head at BOT must not enter item 4's tail route.
    #[test]
    fn parity_map_tail_route_readable_head_never_at_tape_file_zero() {
        let (_, _, payload) = directory_scan_source();
        let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        let mut head = encoded.blocks[0].clone();
        head[0] ^= 1;
        let mut source =
            RecordingRawSource::new(encoded.blocks.iter().cloned().map(Record::Block).collect());
        let mut builder = FilemarkMapBuilder::new();
        let result = append_classified_entry(
            &mut source,
            &mut builder,
            &head,
            &TAPE_UUID,
            BLOCK_SIZE,
            PhysicalPositionHint::new(0),
            encoded.blocks.len() as u64,
            &mut Vec::new(),
        );
        assert!(matches!(
            result,
            Err(ParityError::FilemarkMapReconstruct(_))
        ));
        let reads: Vec<_> = source
            .calls
            .iter()
            .filter_map(|call| match call {
                ScanCall::ReadRecord(lba) => Some(*lba),
                _ => None,
            })
            .collect();
        assert_eq!(reads, vec![encoded.blocks.len() as u64 - 1]);
    }

    /// Even a rejected footer is read just once across all fallback probes.
    #[test]
    fn fallback_probes_share_last_block() {
        for head in [None, Some(0xc0)] {
            let (_, source, start) = parity_map_tail_case(head, true, false, false);
            assert_eq!(
                source
                    .calls
                    .iter()
                    .filter(|call| matches!(call, ScanCall::ReadRecord(lba) if *lba == start + 2))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn parity_map_tail_route_rejects_header_footer_disagreement() {
        let (mut source, map, _) = directory_scan_source();
        let start = map
            .physical_position(TapeFilePosition {
                tape_file_number: 3,
                block_within_file: 0,
            })
            .unwrap();
        let Record::Block(tail) = &mut source.records[start.lba as usize + 1] else {
            panic!("tail fixture")
        };
        tail[0x20] ^= 1; // sequence, with a valid CRC
        let crc = crate::crc64_xz(&tail[..0xc0]);
        tail[0xc0..0xc8].copy_from_slice(&crc.to_le_bytes());
        let footer = read_optional_fixed_block_at(&mut source, start, 2, BLOCK_SIZE).unwrap();
        assert!(!classify_parity_map_from_footer_tail(
            footer.as_deref(),
            &mut source,
            start,
            &TAPE_UUID,
            BLOCK_SIZE,
            3,
            &mut Vec::new()
        )
        .unwrap());
    }

    /// Return the baseline walk independently of the directory reconciliation pass.
    fn first_pass(source: &mut RecordingRawSource) -> FilemarkMap {
        let ScanReconstructionOutcome::Complete(walked) =
            scan_reconstruct_filemark_map_with_provenance(
                source,
                &TAPE_UUID,
                BLOCK_SIZE,
                ScanMode::Standard,
                &mut |_| ScanWalkControl::Continue,
                &mut |_| {},
            )
            .unwrap()
        else {
            panic!("uncontrolled walk must complete")
        };
        walked.map
    }

    /// REM-PARITY 12.3 item 6: a tail header copy that parses but disagrees
    /// with the footer, field for field, means the file is not recognised. The
    /// Scanner reports it and falls through to the next rung, so the file is an
    /// Object candidate for the first pass and the walk is not abandoned; the
    /// final ParityMap's entry then identifies it in the second pass.
    #[test]
    fn sidecar_tail_copy_disagreeing_with_the_footer_falls_through() {
        use crate::sidecar::{encode_sidecar_tape_file, SidecarDescriptor};
        let (mut source, expected, _) = directory_scan_source();
        let other = encode_sidecar_tape_file(
            &SidecarDescriptor {
                tape_uuid: TAPE_UUID,
                epoch_id: 0,
                k: 2,
                m: 1,
                stripes_per_epoch: 1,
                block_size: BLOCK_SIZE,
                protected_ordinal_start: 0,
                protected_ordinal_end_exclusive: 2,
            },
            &[block(0x44)],
            vec![1, 1],
        )
        .unwrap();
        // The sidecar's records follow the bootstrap, the Object and their
        // two filemarks. The other sidecar's tail copy is valid on its own
        // and differs from this one's footer in the canonical hash.
        let sidecar_start = 5;
        let tail = usize::try_from(other.header.tail_header_start_block).unwrap();
        let h = usize::try_from(other.header.shard_index_block_count).unwrap();
        // Item 6 is reached only when the primary's head block is unreadable
        // (item 5 recognises a file whose primary header parses).
        source.records[sidecar_start] = Record::ReadFault(TestReadFault::Medium);
        for i in 0..h {
            source.records[sidecar_start + tail + i] =
                Record::Block(other.blocks[tail + i].clone());
        }
        let walked = scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("one tape file's disagreement does not abandon the walk");
        assert_eq!(
            walked
                .damaged_regions
                .iter()
                .map(|region| region.kind)
                .collect::<Vec<_>>(),
            vec![
                ScanDamageKind::UnreadableTapeFileHead,
                ScanDamageKind::ClassificationTailMismatch
            ]
        );
        // The first pass falls through to an Object candidate (item 7) ...
        let mut first = directory_scan_source().0;
        first.records[sidecar_start] = Record::ReadFault(TestReadFault::Medium);
        for i in 0..h {
            first.records[sidecar_start + tail + i] = Record::Block(other.blocks[tail + i].clone());
        }
        let first_pass_map = first_pass(&mut first);
        assert_eq!(first_pass_map.entries()[2].kind, TapeFileKind::Object);
        // ... and the second pass restores the sidecar from the directory.
        assert_eq!(walked.map, expected);
        // A tail copy that reads but is invalid is not unreadable either: the
        // file is not recognised from the footer alone.
        let (mut invalid, _, _) = directory_scan_source();
        invalid.records[sidecar_start] = Record::ReadFault(TestReadFault::Medium);
        if let Record::Block(block) = &mut invalid.records[sidecar_start + tail] {
            block[0xd0] ^= 0x01;
        }
        let first = first_pass(&mut invalid);
        assert_eq!(first.entries()[2].kind, TapeFileKind::Object);
        // An unreadable tail copy is not a disagreement: the footer classifies.
        let (mut unreadable, _, _) = directory_scan_source();
        unreadable.records[sidecar_start] = Record::ReadFault(TestReadFault::Medium);
        unreadable.records[sidecar_start + tail] = Record::ReadFault(TestReadFault::Medium);
        let walked =
            scan_reconstruct_filemark_map_with_report(&mut unreadable, &TAPE_UUID, BLOCK_SIZE)
                .unwrap();
        assert_eq!(
            walked
                .damaged_regions
                .iter()
                .map(|region| region.kind)
                .collect::<Vec<_>>(),
            vec![ScanDamageKind::UnreadableTapeFileHead]
        );
        assert_eq!(walked.map, expected);
    }

    /// Build a parsed authority with a pending ordinal and a later unvalidated
    /// Object. No media reads are needed to exercise the Recoverer's fences.
    fn walked_scope_authority() -> (FilemarkMap, crate::DecodedParityMapTapeFile) {
        let (_, original, mut payload) = directory_scan_source();
        let mut entries = original.entries().to_vec();
        entries[1].block_count = 3;
        let prefix = FilemarkMap::new(entries.clone()).unwrap();
        payload.directory.directory_scope_tape_file_count = 4;
        payload.directory.directory_scope_total_data_ordinals = 3;
        payload.canonical_map_digest = prefix.canonical_digest().unwrap();
        let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        assert_eq!(encoded.blocks.len() as u64, entries[3].block_count);
        let decoded = crate::parse_parity_map_tape_file(&encoded.blocks, &TAPE_UUID).unwrap();
        entries.push(TapeFileMapEntry::object(4, 2, 3));
        (FilemarkMap::new(entries).unwrap(), decoded)
    }

    #[test]
    fn walked_scope_rejects_object_mismatch_and_parity_map_block_count_precheck() {
        let (map, decoded) = walked_scope_authority();
        for index in [1, 3] {
            let mut entries = map.entries().to_vec();
            entries[index].block_count += 1;
            if index == 1 {
                entries[4].first_parity_data_ordinal = Some(4);
            }
            let changed = FilemarkMap::new(entries).unwrap();
            assert!(matches!(
                ScopedFilemarkMap::validate_against_final_parity_map(changed, &decoded),
                Err(ParityError::FilemarkMapDigestMismatch { .. })
            ));
        }
    }

    /// A validated final ParityMap with a mismatched projection refuses the
    /// production walk route even when the bootstrap would attest its BOT prefix.
    #[test]
    fn walked_scope_digest_mismatch_refuses_without_bootstrap_fallback() {
        for mismatch in [false, true] {
            let (mut source, expected, mut payload) = directory_scan_source();
            payload.directory.directory_scope_tape_file_count = expected.tape_file_count();
            payload.canonical_map_digest = expected.canonical_digest().unwrap();
            if mismatch {
                payload.canonical_map_digest[0] ^= 1;
            }
            let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
            assert_eq!(
                encoded.blocks.len() as u64,
                expected.entries()[3].block_count
            );
            let start = expected
                .physical_position(TapeFilePosition {
                    tape_file_number: 3,
                    block_within_file: 0,
                })
                .unwrap()
                .lba as usize;
            for (offset, block) in encoded.blocks.into_iter().enumerate() {
                source.records[start + offset] = Record::Block(block);
            }
            let walked =
                scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
                    .unwrap();
            assert_eq!(walked.map, expected);
            let authority = read_final_parity_map(&mut source, &walked.map, &TAPE_UUID, BLOCK_SIZE)
                .unwrap()
                .expect("encoded final ParityMap must validate despite the projection mismatch");
            assert_eq!(authority.payload, payload);
            let bootstrap = walked.authoritative_bootstrap().unwrap().payload.clone();
            let fallback = ScopedFilemarkMap::validate_against_digest(
                walked.map.clone(),
                bootstrap.filemark_map_digest.as_ref().unwrap(),
            )
            .expect("bootstrap scope would succeed if incorrectly used as fallback");
            assert_eq!(fallback.validated_prefix_tape_files, Some(1));
            assert_eq!(fallback.scope.watermark(), 0);
            let result = validate_scan_reconstruction_with_report(&mut source, &bootstrap, walked);
            if mismatch {
                assert!(matches!(
                    result,
                    Err(ParityError::FilemarkMapDigestMismatch {
                        truncation_position: None
                    })
                ));
            } else {
                let scoped = result.unwrap().scoped_map;
                assert_eq!(scoped.validated_prefix_tape_files, Some(4));
                assert_eq!(scoped.scope.watermark(), 2);
                assert_eq!(scoped.sidecar_directory, Some(payload.directory));
            }
        }
    }

    #[test]
    fn walked_scope_requires_final_mark_and_matching_scope_scalars() {
        let (map, decoded) = walked_scope_authority();
        for mutation in ["not-final", "scope", "total", "watermark"] {
            let mut payload = decoded.payload.clone();
            match mutation {
                "not-final" => payload.directory.is_final_directory = false,
                "scope" => payload.directory.directory_scope_tape_file_count -= 1,
                "total" => payload.directory.directory_scope_total_data_ordinals += 1,
                "watermark" => {
                    payload.directory.directory_scope_highest_protected_ordinal -= 1;
                    payload.directory.entries[0].protected_ordinal_end_exclusive -= 1;
                }
                _ => unreachable!(),
            }
            let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
            let authority = crate::parse_parity_map_tape_file(&encoded.blocks, &TAPE_UUID).unwrap();
            assert!(matches!(
                ScopedFilemarkMap::validate_against_final_parity_map(map.clone(), &authority),
                Err(ParityError::FilemarkMapDigestMismatch { .. })
            ));
        }
    }

    #[test]
    fn walked_scope_fences_pending_and_outside_ordinals_before_io() {
        let (map, decoded) = walked_scope_authority();
        let scoped = ScopedFilemarkMap::validate_against_final_parity_map(map, &decoded).unwrap();
        assert_eq!(scoped.validated_prefix_tape_files, Some(4));
        assert_eq!(scoped.scope.watermark(), 2);
        assert_eq!(scoped.sidecar_directory, Some(decoded.payload.directory));
        let boundary = crate::durable::DurableBoundaryState::from_scoped_map(&scoped).unwrap();
        assert!(boundary.contains_committed_tape_file(3));
        assert!(!boundary.contains_committed_tape_file(4));
        let mut source = RecordingRawSource::new(Vec::new());
        for ordinal in [2, 3, 4] {
            let result = crate::recover_ordinal_from_sidecar(
                &mut source,
                &scoped,
                &sample_scheme(),
                TAPE_UUID,
                BLOCK_SIZE,
                ordinal,
            );
            if ordinal == 2 {
                assert!(matches!(
                    result,
                    Err(ParityError::UnrecoverablePendingEpoch {
                        failed_ordinal: 2,
                        watermark: 2
                    })
                ));
            } else {
                assert!(matches!(
                    result,
                    Err(ParityError::OutsideValidatedMapPrefix {
                        ordinal: failed,
                        prefix_ordinals: 3
                    }) if failed == ordinal
                ));
            }
        }
        assert!(source.calls.is_empty(), "scope refusals precede tape I/O");
    }

    #[test]
    fn walked_scope_without_validated_final_parity_map_retains_bootstrap_scope() {
        for damage in ["not-final", "both-copies"] {
            let (mut source, expected, mut payload) = directory_scan_source();
            let start = expected
                .physical_position(TapeFilePosition {
                    tape_file_number: 3,
                    block_within_file: 0,
                })
                .unwrap()
                .lba as usize;
            if damage == "not-final" {
                payload.directory.is_final_directory = false;
                let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
                for (offset, block) in encoded.blocks.into_iter().enumerate() {
                    source.records[start + offset] = Record::Block(block);
                }
            } else {
                for record in &mut source.records[start..] {
                    if matches!(record, Record::Block(_)) {
                        *record = Record::ReadFault(TestReadFault::Medium);
                    }
                }
            }
            let walked =
                scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
                    .unwrap();
            let bootstrap = walked.authoritative_bootstrap().unwrap().payload.clone();
            let scoped = validate_scan_reconstruction_with_report(&mut source, &bootstrap, walked)
                .unwrap()
                .scoped_map;
            assert_eq!(scoped.validated_prefix_tape_files, Some(1));
            assert_eq!(scoped.scope.watermark(), 0);
            assert!(scoped.sidecar_directory.is_none());
        }
    }

    #[test]
    fn directory_walk_healthy_sidecars_add_only_the_parity_map_load() {
        let (mut baseline, expected, _) = directory_scan_source();
        let (mut source, _, _) = directory_scan_source();
        assert_eq!(first_pass(&mut baseline), expected);
        assert!(
            read_final_sidecar_directory(&mut baseline, &expected, &TAPE_UUID, BLOCK_SIZE)
                .unwrap()
                .is_some()
        );
        let walked =
            scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();
        assert_eq!(walked.map, expected);
        assert_eq!(
            source.calls, baseline.calls,
            "no sidecar tail I/O after the walk"
        );
        let bootstrap = walked.authoritative_bootstrap().unwrap().payload.clone();
        source.calls.clear();
        for wrong_identity in [true, false] {
            let mut conflicting = bootstrap.clone();
            if wrong_identity {
                conflicting.tape_uuid[0] ^= 1;
            } else {
                conflicting.block_size_bytes *= 2;
            }
            assert!(matches!(
                validate_scan_reconstruction_with_report(&mut source, &conflicting, walked.clone()),
                Err(ParityError::FilemarkMapReconstruct(_))
            ));
        }
        validate_scan_reconstruction_with_report(&mut source, &bootstrap, walked).unwrap();
        assert!(
            source.calls.is_empty(),
            "validation reuses the loaded ParityMap"
        );
    }

    /// A lost filemark shifts a real Object onto a directory sidecar's number
    /// and length. Unconfirmed identification must not hide it from BOT inventory.
    #[test]
    fn directory_walk_lost_filemark_keeps_real_object_in_bot_inventory() {
        let (mut source, original, mut payload) = directory_scan_source();
        let object_blocks = original.entries()[2].block_count;
        let mut entries = original.entries().to_vec();
        entries.insert(3, TapeFileMapEntry::object(3, object_blocks, 2));
        entries[4].tape_file_number = 4;
        let intact = FilemarkMap::new(entries).unwrap();
        payload.directory.directory_scope_tape_file_count = 5;
        payload.directory.directory_scope_total_data_ordinals = 2 + object_blocks;
        payload.canonical_map_digest = intact.canonical_digest().unwrap();
        let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        assert_eq!(
            encoded.blocks.len() as u64,
            original.entries()[3].block_count
        );
        let map_start = original
            .physical_position(TapeFilePosition {
                tape_file_number: 3,
                block_within_file: 0,
            })
            .unwrap()
            .lba as usize;
        source.records.truncate(map_start);
        source
            .records
            .extend((0..object_blocks).map(|_| Record::Block(block(0x55))));
        source.records.push(Record::Filemark);
        source
            .records
            .extend(encoded.blocks.into_iter().map(Record::Block));
        source.records.push(Record::Filemark);
        // Keep the directory scope within the walk despite the missing boundary.
        source
            .records
            .extend([Record::Block(block(0x66)), Record::Filemark]);
        assert!(matches!(source.records.remove(4), Record::Filemark));

        let measured = first_pass(&mut source);
        assert_eq!(measured.entries()[2].kind, TapeFileKind::Object);
        assert_eq!(
            measured.entries()[2].block_count,
            payload.directory.entries[0].sidecar_total_block_count
        );
        assert_eq!(measured.entries()[3].kind, TapeFileKind::ParityMap);
        assert_eq!(
            measured.tape_file_count(),
            payload.directory.directory_scope_tape_file_count
        );
        let walked =
            scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();
        assert_eq!(walked.map, measured);
        let bootstrap = walked.authoritative_bootstrap().unwrap().payload.clone();
        assert!(matches!(
            validate_scan_reconstruction_with_report(&mut source, &bootstrap, walked),
            Err(ParityError::FilemarkMapDigestMismatch { .. })
        ));
        let mut objects = Vec::new();
        crate::recover_terminal_inventory_from_bot(&mut source, &TAPE_UUID, BLOCK_SIZE, |object| {
            objects.push((object.tape_file_number, object.stored_block_count));
            Ok(())
        })
        .unwrap();
        assert!(
            objects.contains(&(2, Some(object_blocks))),
            "real Object must remain in BOT inventory"
        );
    }

    /// Directory matches are provisional until all projection and scope fields
    /// validate, even when the ParityMap itself has valid metadata checksums.
    #[test]
    fn directory_walk_keeps_objects_when_confirmation_fails() {
        for mismatch in ["digest", "scope", "total", "watermark"] {
            let (mut source, expected, mut payload) = directory_scan_source();
            match mismatch {
                "digest" => payload.canonical_map_digest[0] ^= 1,
                "scope" => payload.directory.directory_scope_tape_file_count -= 1,
                "total" => payload.directory.directory_scope_total_data_ordinals += 1,
                "watermark" => {
                    payload.directory.directory_scope_highest_protected_ordinal -= 1;
                    payload.directory.entries[0].protected_ordinal_end_exclusive -= 1;
                }
                _ => unreachable!(),
            }
            let end = 5 + expected.entries()[2].block_count as usize;
            source.records[5] = Record::ReadFault(TestReadFault::Medium);
            source.records[end - 1] = Record::ReadFault(TestReadFault::Medium);
            let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
            assert_eq!(
                encoded.blocks.len() as u64,
                expected.entries()[3].block_count
            );
            for (offset, block) in encoded.blocks.into_iter().enumerate() {
                source.records[end + 1 + offset] = Record::Block(block);
            }
            let measured = first_pass(&mut source);
            assert_eq!(measured.entries()[2].kind, TapeFileKind::Object);
            let walked =
                scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
                    .unwrap();
            assert_eq!(walked.map, measured, "{mismatch}");
            let bootstrap = walked.authoritative_bootstrap().unwrap().payload.clone();
            assert!(
                matches!(
                    validate_scan_reconstruction_with_report(&mut source, &bootstrap, walked),
                    Err(ParityError::FilemarkMapDigestMismatch { .. })
                ),
                "{mismatch}"
            );
        }
    }

    /// A final directory extending beyond the walk is a refusal, not permission
    /// to recover under the otherwise valid bootstrap prefix.
    #[test]
    fn walked_scope_exceeding_map_refuses_without_bootstrap_fallback() {
        let (mut source, expected, mut payload) = directory_scan_source();
        payload.directory.directory_scope_tape_file_count = expected.tape_file_count() + 1;
        let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        assert_eq!(
            encoded.blocks.len() as u64,
            expected.entries()[3].block_count
        );
        let start = expected
            .physical_position(TapeFilePosition {
                tape_file_number: 3,
                block_within_file: 0,
            })
            .unwrap()
            .lba as usize;
        for (offset, block) in encoded.blocks.into_iter().enumerate() {
            source.records[start + offset] = Record::Block(block);
        }
        assert!(matches!(
            read_final_parity_map(&mut source, &expected, &TAPE_UUID, BLOCK_SIZE),
            Err(ParityError::FilemarkMapDigestMismatch { .. })
        ));
        assert!(matches!(
            scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE),
            Err(ParityError::FilemarkMapDigestMismatch { .. })
        ));
    }

    #[test]
    fn directory_walk_preserves_disagreeing_classified_sidecar_without_rescue() {
        let (mut source, expected, mut payload) = directory_scan_source();
        payload.directory.entries[0].protected_ordinal_end_exclusive = 1;
        payload.directory.directory_scope_highest_protected_ordinal = 1;
        let encoded = crate::encode_parity_map_tape_file(&payload, BLOCK_SIZE).unwrap();
        let start = expected
            .physical_position(TapeFilePosition {
                tape_file_number: 3,
                block_within_file: 0,
            })
            .unwrap()
            .lba as usize;
        for (offset, block) in encoded.blocks.into_iter().enumerate() {
            source.records[start + offset] = Record::Block(block);
        }
        let mut baseline = RecordingRawSource::new(source.records.clone());
        assert_eq!(first_pass(&mut baseline), expected);
        let directory =
            read_final_sidecar_directory(&mut baseline, &expected, &TAPE_UUID, BLOCK_SIZE)
                .unwrap()
                .unwrap();
        let walked =
            scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();
        assert_eq!(walked.map, expected);
        assert_eq!(source.calls, baseline.calls);
        source.calls.clear();
        assert!(matches!(
            read_directory_tail_index(
                &mut source,
                &walked.map,
                &walked.map.entries()[2],
                &directory.entries[0],
                &TAPE_UUID,
                BLOCK_SIZE,
                crate::recovery::scheme_parity_blocks(&sample_scheme())
            ),
            Err(ParityError::SidecarMetadataUnavailable { .. })
        ));
        assert!(
            source.calls.is_empty(),
            "recovery rejects disagreement before I/O"
        );
    }

    #[test]
    fn directory_walk_aborted_after_parity_map_runs_no_second_pass() {
        let (mut source, _, _) = directory_scan_source();
        let (mut baseline, _, _) = directory_scan_source();
        let control = |event: &ScanWalkProgress| {
            if event.tape_file_number == 3 {
                ScanWalkControl::Abort
            } else {
                ScanWalkControl::Continue
            }
        };
        assert!(matches!(
            scan_reconstruct_filemark_map_with_provenance(
                &mut baseline,
                &TAPE_UUID,
                BLOCK_SIZE,
                ScanMode::Standard,
                &mut { control },
                &mut |_| {},
            )
            .unwrap(),
            ScanReconstructionOutcome::Aborted(_)
        ));
        assert!(matches!(
            scan_reconstruct_filemark_map_with_control(
                &mut source,
                &TAPE_UUID,
                BLOCK_SIZE,
                control
            )
            .unwrap(),
            ControlledScanWalkOutcome::Aborted(_)
        ));
        assert_eq!(source.calls, baseline.calls);
    }

    #[test]
    fn directory_walk_identifies_by_measured_length_without_tail_io() {
        for extra_blocks in [0, 1] {
            let (mut source, expected, payload) = directory_scan_source();
            let mut entries = expected.entries().to_vec();
            let sidecar = &entries[2];
            let end = 5 + sidecar.block_count as usize;
            // Hide primary/footer recognition but retain the exact valid tail.
            source.records[5] = Record::ReadFault(TestReadFault::Medium);
            source.records[end - 1] = Record::ReadFault(TestReadFault::Medium);
            if extra_blocks != 0 {
                source.records.insert(end, Record::Block(block(0x99)));
            }
            entries[2] = TapeFileMapEntry::object(2, sidecar.block_count + extra_blocks, 2);
            let measured = first_pass(&mut source);
            assert_eq!(measured, FilemarkMap::new(entries).unwrap());
            let entry = &payload.directory.entries[0];
            assert_eq!(
                entry.sidecar_total_block_count,
                2 * entry.sidecar_header_block_count + entry.parity_shard_block_count + 1
            );
            let tail = 5 + entry.sidecar_header_block_count + entry.parity_shard_block_count;
            // Prove the tail is readable and hash-valid even in the length-mismatch case.
            let decoded = read_directory_tail_index(
                &mut source,
                &measured,
                &expected.entries()[2],
                entry,
                &TAPE_UUID,
                BLOCK_SIZE,
                crate::recovery::scheme_parity_blocks(&sample_scheme()),
            )
            .unwrap();
            assert_eq!(
                decoded.header.canonical_metadata_hash,
                entry.canonical_metadata_hash
            );
            source.calls.clear();
            let mut reconciled = measured.clone();
            reconcile_walk_sidecars(&mut source, &mut reconciled, &TAPE_UUID, BLOCK_SIZE).unwrap();
            if extra_blocks == 0 {
                assert_eq!(reconciled, expected);
            } else {
                assert_eq!(reconciled, measured);
            }
            assert!(!source.calls.contains(&ScanCall::Locate(tail)));
            assert!(!source.calls.contains(&ScanCall::ReadRecord(tail)));
        }
    }

    #[test]
    fn directory_probes_degrade_only_current_medium_reads() {
        for probe in ["directory", "tail", "rescue-directory", "rescue-tail"] {
            let loader = probe.ends_with("directory");
            let rescue = probe.starts_with("rescue-");
            for locate in [false, true] {
                for fault in [
                    TestReadFault::Medium,
                    TestReadFault::DeferredFixedMedium,
                    TestReadFault::DeferredDescriptorMedium,
                    TestReadFault::Hardware,
                    TestReadFault::Transport,
                ] {
                    let (mut source, map, payload) = directory_scan_source();
                    let mut entries = map.entries().to_vec();
                    entries[2] = TapeFileMapEntry::object(2, entries[2].block_count, 2);
                    let measured = FilemarkMap::new(entries).unwrap();
                    let entry = &payload.directory.entries[0];
                    let tail =
                        5 + entry.sidecar_header_block_count + entry.parity_shard_block_count;
                    let target = if loader {
                        map.physical_position(TapeFilePosition {
                            tape_file_number: 3,
                            block_within_file: 0,
                        })
                        .unwrap()
                        .lba
                    } else {
                        tail
                    };
                    if rescue {
                        // Force the public recovery call through its directory rescue.
                        // The footer is unreadable too: after a valid footer the
                        // directory is not used to rescue (REM-PARITY 13.3).
                        source.records[5] = Record::ReadFault(TestReadFault::Medium);
                        source.records[5 + map.entries()[2].block_count as usize - 1] =
                            Record::ReadFault(TestReadFault::Medium);
                    }
                    if locate {
                        source.locate_fault = Some((target, fault));
                    } else if loader {
                        // Both copies unavailable: the directory must be absent.
                        for record in &mut source.records[target as usize..] {
                            if matches!(record, Record::Block(_)) {
                                *record = Record::ReadFault(fault);
                            }
                        }
                    } else {
                        source.records[target as usize] = Record::ReadFault(fault);
                    }
                    let result = if rescue {
                        crate::recover_ordinal_from_sidecar(
                            &mut source,
                            &ScopedFilemarkMap::from_catalog(map, 2),
                            &sample_scheme(),
                            TAPE_UUID,
                            BLOCK_SIZE,
                            0,
                        )
                        .map(|_| ())
                    } else if loader {
                        read_final_sidecar_directory(&mut source, &measured, &TAPE_UUID, BLOCK_SIZE)
                            .map(|directory| assert!(directory.is_none()))
                    } else {
                        read_directory_tail_index(
                            &mut source,
                            &measured,
                            &map.entries()[2],
                            entry,
                            &TAPE_UUID,
                            BLOCK_SIZE,
                            crate::recovery::scheme_parity_blocks(&sample_scheme()),
                        )
                        .map(|_| ())
                    };
                    if !locate && fault == TestReadFault::Medium {
                        if loader && !rescue {
                            result
                                .expect("current-medium READ damage leaves directory unavailable");
                        } else {
                            assert!(matches!(
                                result,
                                Err(ParityError::SidecarMetadataUnavailable { epoch_id: 0 })
                            ));
                        }
                    } else {
                        assert_eq!(result.unwrap_err().to_string(), fault.error().to_string());
                    }
                    assert!(source.calls.contains(&ScanCall::Locate(target)));
                    assert_eq!(
                        source.calls.contains(&ScanCall::ReadRecord(target)),
                        !locate
                    );
                }
            }
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

        assert!(matches!(err, ParityError::DriveCompressionEnabled { .. }));
        assert_eq!(err.to_string(), "tape's bootstrap records drive compression; a parity tape must not record drive compression");
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

    /// REM-PARITY 2.4 and 12.2: the walk's count comes from device-reported
    /// positions, so a post-space position before the file's start, or equal
    /// to it, faults the device: `TapeIo`, never a wrapped count, never a
    /// zero-block file and never a map error.
    #[test]
    fn walk_measurement_faults_the_device_for_positions_that_go_nowhere() {
        for start in [5, 1] {
            let mut source = RecordingRawSource::new(vec![Record::Filemark]);
            let error = measure_current_file(&mut source, PhysicalPositionHint::new(start))
                .expect_err("a delta that does not fit is refused");
            assert!(
                matches!(error, ParityError::TapeIo(TapeIoError::OperationFailed(ref m)) if m.starts_with("device-reported position")),
                "start {start}: {error:?}"
            );
        }
    }

    /// REM-PARITY 3.5: a head record shorter or longer than one block is a fact
    /// about the tape. After BOT it is an invalid candidate for every control
    /// rung: the walk classifies the file as for an unreadable head, notes the
    /// damage, and continues. At BOT, with the block size known, it is
    /// `BootstrapParse`. A probed last record of the wrong length is no
    /// candidate either. None of these is `TapeIo`.
    #[test]
    fn walk_treats_records_of_the_wrong_length_as_invalid_content() {
        let bot_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
            .expect("BOT-only map validates");
        let bot = bootstrap_block(bot_map.digest(false).expect("BOT digest"), 0);
        let size = BLOCK_SIZE as usize;
        for head in [size - 1, 40, size + 1, 2 * size] {
            let mut source = RecordingRawSource::new(vec![
                Record::Block(bot.clone()),
                Record::Filemark,
                Record::Block(vec![0x5a; head]),
                Record::Block(block(7)),
                Record::Filemark,
            ]);
            let walked =
                scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
                    .unwrap_or_else(|e| panic!("head of {head} bytes aborted the walk: {e:?}"));
            assert_eq!(walked.map.entries()[1].kind, TapeFileKind::Object, "{head}");
            assert_eq!(walked.map.entries()[1].block_count, 2, "{head}");
            assert!(
                walked
                    .damaged_regions
                    .iter()
                    .any(|region| region.start.lba == 2
                        && region.kind == ScanDamageKind::WrongLengthTapeFileHead),
                "{head}: {:?}",
                walked.damaged_regions
            );
        }
        for first in [size - 1, 2 * size] {
            let mut source =
                RecordingRawSource::new(vec![Record::Block(vec![0; first]), Record::Filemark]);
            let error =
                scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
                    .expect_err("a first record of another length is refused");
            assert!(
                matches!(error, ParityError::BootstrapParse(_)),
                "{first}: {error:?}"
            );
        }
        // An unreadable head sends the walk to the footer probes; a last record
        // of the wrong length there is no candidate, so the file stays an
        // Object candidate.
        for last in [size - 1, 2 * size] {
            let mut source = RecordingRawSource::new(vec![
                Record::Block(bot.clone()),
                Record::Filemark,
                Record::ReadFault(TestReadFault::Medium),
                Record::Block(vec![0x5a; last]),
                Record::Filemark,
            ]);
            let walked =
                scan_reconstruct_filemark_map_with_report(&mut source, &TAPE_UUID, BLOCK_SIZE)
                    .unwrap_or_else(|e| panic!("probe of {last} bytes aborted the walk: {e:?}"));
            assert_eq!(walked.map.entries()[1].kind, TapeFileKind::Object, "{last}");
        }
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
            Err(ParityError::DriveCompressionEnabled { .. })
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

    /// Nonconformance cannot conceal authenticated scheme or compression
    /// conflicts; a scheme record on a no-parity bootstrap is one (Section 8.4).
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
                        assert!(matches!(
                            result,
                            Err(ParityError::DriveCompressionEnabled { .. })
                        ))
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

    /// Supplied values that agree with the parity bootstrap of these tests.
    fn matching_hints(payload: &BootstrapPayload) -> ScanRecoveryHints {
        let record = payload.scheme.as_ref().expect("parity bootstrap");
        ScanRecoveryHints {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            scheme: crate::ParityConfig::Scheme(ParityScheme {
                id: SchemeId::new_owned(record.id.clone()),
                data_blocks_per_stripe: record.data_blocks_per_stripe,
                parity_blocks_per_stripe: record.parity_blocks_per_stripe,
                stripes_per_neighborhood: record.stripes_per_neighborhood,
            }),
        }
    }

    fn refused_field<T: std::fmt::Debug>(result: Result<T, ParityError>) -> BootstrapRefusedField {
        match result {
            Err(ParityError::BootstrapRefused { field, .. }) => field,
            other => panic!("expected a typed bootstrap refusal, got {other:?}"),
        }
    }

    fn with_header_crc(mut block: Vec<u8>) -> Vec<u8> {
        let crc = crate::crc64_xz(&block[..48]);
        block[48..56].copy_from_slice(&crc.to_le_bytes());
        block
    }

    /// The measured length is compared with the supplied size, never with the read size.
    #[test]
    fn check_bootstrap_read_length_compares_with_the_supplied_size() {
        let hints = ScanRecoveryHints {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            scheme: crate::ParityConfig::None,
        };
        let size = BLOCK_SIZE as usize;
        let read = |bytes| {
            Ok(RawReadOutcome::Block {
                bytes,
                position_after: PhysicalPositionHint::new(1),
            })
        };
        let longer = |actual: usize, provided: usize| {
            Err(ParityError::TapeIo(TapeIoError::ReadBufferTooSmall {
                actual: actual as u32,
                provided: provided as u32,
            }))
        };
        // At the supplied size, any other measured length is refused.
        hints
            .check_bootstrap_read_length(&read(size), size)
            .expect("a full record");
        for (outcome, read_size) in [
            (read(size - 1), size),
            (read(40), size),
            (longer(2 * size, size), size),
        ] {
            assert_eq!(
                refused_field(hints.check_bootstrap_read_length(&outcome, read_size)),
                BootstrapRefusedField::BlockSize
            );
        }
        let error = hints
            .check_bootstrap_read_length(&read(40), size)
            .expect_err("short record");
        assert_eq!(error.to_string(), format!("bootstrap refused: short fixed-block bootstrap read: got 40 bytes, supplied block size is {BLOCK_SIZE}"));
        // At another candidate size, a record of the supplied length only rules
        // that candidate out; one of any other length is still refused.
        hints
            .check_bootstrap_read_length(&read(size), 2 * size)
            .expect("a short read of the supplied length");
        hints
            .check_bootstrap_read_length(&longer(size, size / 2), size / 2)
            .expect("an over-length read of the supplied length");
        for (outcome, read_size) in [(read(40), 2 * size), (longer(2 * size, size / 2), size / 2)] {
            assert_eq!(
                refused_field(hints.check_bootstrap_read_length(&outcome, read_size)),
                BootstrapRefusedField::BlockSize
            );
        }
        // Every other outcome is left to the caller.
        for outcome in [
            Ok(RawReadOutcome::Filemark {
                position_after: PhysicalPositionHint::new(1),
            }),
            Ok(RawReadOutcome::EndOfData {
                position_after: PhysicalPositionHint::new(0),
            }),
            Err(TestReadFault::Medium.error()),
            Err(TestReadFault::Transport.error()),
        ] {
            hints
                .check_bootstrap_read_length(&outcome, size)
                .expect("not a length judgement");
        }
    }

    /// The walk's BOT read judges the record's length before its content.
    #[test]
    fn scanner_recovery_refuses_first_record_length_before_content() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("map");
        let payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        let hints = matching_hints(&payload);
        let valid = bootstrap_block_for_payload(&payload);
        let mut no_magic = valid[..40].to_vec();
        no_magic[0] ^= 1;
        let mut longer = valid.clone();
        longer.resize(2 * BLOCK_SIZE as usize, 0);
        for first in [valid[..valid.len() - 1].to_vec(), no_magic, longer] {
            let records = vec![
                Record::Block(first),
                Record::Filemark,
                Record::Block(block(0x33)),
                Record::Filemark,
            ];
            let mut source = RecordingRawSource::new(records.clone());
            let result = scan_reconstruct_filemark_map_with_report_mode(
                &mut source,
                &TAPE_UUID,
                BLOCK_SIZE,
                ScanMode::Recovery(&hints),
            );
            assert_eq!(refused_field(result), BootstrapRefusedField::BlockSize);
            assert_eq!(
                source
                    .calls
                    .iter()
                    .filter(|call| matches!(call, ScanCall::ReadRecord(_)))
                    .count(),
                1,
                "the refusal stops the walk at the first record"
            );
            // Without supplied values the length is the walk's own concern.
            let error = scan_reconstruct_filemark_map_with_report(
                &mut RecordingRawSource::new(records),
                &TAPE_UUID,
                BLOCK_SIZE,
            )
            .expect_err("a first record of the wrong length");
            assert!(
                !matches!(error, ParityError::BootstrapRefused { .. }),
                "{error}"
            );
        }
    }

    /// A filemark or EOD where the first record should be is unreadable, not
    /// refused: the walk continues and fails only for want of tape file 0.
    #[test]
    fn scanner_recovery_treats_a_missing_first_record_as_unreadable() {
        let hints = ScanRecoveryHints {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            scheme: crate::ParityConfig::None,
        };
        for records in [
            vec![
                Record::Filemark,
                Record::Block(block(0x33)),
                Record::Filemark,
            ],
            Vec::new(),
        ] {
            let recovery = scan_reconstruct_filemark_map_with_report_mode(
                &mut RecordingRawSource::new(records.clone()),
                &TAPE_UUID,
                BLOCK_SIZE,
                ScanMode::Recovery(&hints),
            )
            .expect_err("no tape file 0 to map");
            let standard = scan_reconstruct_filemark_map_with_report(
                &mut RecordingRawSource::new(records),
                &TAPE_UUID,
                BLOCK_SIZE,
            )
            .expect_err("no tape file 0 to map");
            assert!(
                matches!(&recovery, ParityError::FilemarkMapReconstruct(_)),
                "{recovery}"
            );
            assert_eq!(recovery.to_string(), standard.to_string());
        }
    }

    /// Each refusal names the field Section 8.4 names, first in its order, and
    /// survives a damaged payload.
    #[test]
    fn classify_bootstrap_names_the_first_refused_field() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("map");
        let payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        let hints = matching_hints(&payload);
        let valid = bootstrap_block_for_payload(&payload);
        let edit = |edits: &[(usize, Vec<u8>)], damaged_payload: bool| {
            let mut block = valid.clone();
            for (offset, bytes) in edits {
                block[*offset..*offset + bytes.len()].copy_from_slice(bytes);
            }
            if damaged_payload {
                block[80] ^= 1;
            }
            with_header_crc(block)
        };
        let major = (8, 3u16.to_be_bytes().to_vec());
        let uuid = (16, vec![0x99; 16]);
        let size = (32, (2 * BLOCK_SIZE).to_be_bytes().to_vec());
        let sequence = (36, 1u64.to_be_bytes().to_vec());
        let flag = (12, crate::bootstrap::FLAG_NO_PARITY.to_be_bytes().to_vec());
        for (edits, field) in [
            (vec![major.clone()], BootstrapRefusedField::FormatMajor),
            (vec![uuid.clone()], BootstrapRefusedField::TapeUuid),
            (vec![size.clone()], BootstrapRefusedField::BlockSize),
            (vec![sequence.clone()], BootstrapRefusedField::Sequence),
            (vec![flag.clone()], BootstrapRefusedField::NoParityFlag),
            (
                vec![flag, sequence, size.clone(), uuid.clone(), major],
                BootstrapRefusedField::FormatMajor,
            ),
            (vec![size, uuid], BootstrapRefusedField::TapeUuid),
        ] {
            for damaged_payload in [false, true] {
                let error = hints
                    .classify_bootstrap(&edit(&edits, damaged_payload))
                    .expect_err("header refusal");
                let detail = match field {
                    BootstrapRefusedField::FormatMajor => "unsupported bootstrap schema major version: got 3, accept 2",
                    BootstrapRefusedField::TapeUuid => "tape identity mismatch: readable bootstrap header differs from supplied hints",
                    BootstrapRefusedField::BlockSize => "readable bootstrap block size differs from supplied hints",
                    BootstrapRefusedField::Sequence => "schema-major 2 permits only the sequence-0 BOT Bootstrap: got sequence 1",
                    BootstrapRefusedField::NoParityFlag => "readable bootstrap parity scheme differs from supplied hints: no-parity flag contradicts scheme",
                    BootstrapRefusedField::Scheme => unreachable!(),
                };
                assert_eq!(error.to_string(), format!("bootstrap refused: {detail}"));
                assert_eq!(refused_field::<RecoveryBootstrap>(Err(error)), field);
            }
        }
        let mut other_scheme = hints.clone();
        let crate::ParityConfig::Scheme(scheme) = &mut other_scheme.scheme else {
            panic!("parity hints")
        };
        scheme.parity_blocks_per_stripe += 1;
        assert_eq!(
            refused_field(other_scheme.classify_bootstrap(&valid)),
            BootstrapRefusedField::Scheme
        );
        assert!(matches!(
            hints.classify_bootstrap(&valid),
            Ok(RecoveryBootstrap::Validated(_))
        ));
    }

    /// The salvage path judges recorded compression before the scheme, and only
    /// on a parity tape, as the parser does.
    #[test]
    fn recovery_salvage_judges_compression_first_and_only_on_a_parity_tape() {
        use ciborium::value::Value;
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("map");
        let payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        let hints = matching_hints(&payload);
        // A noncanonical payload (keys out of order) with key 5 true, as a
        // parity or a no-parity bootstrap, with or without the scheme.
        let build = |no_parity: bool, keep_scheme: bool| {
            let mut block = bootstrap_block_for_payload(&payload);
            let len = u32::from_le_bytes(block[44..48].try_into().unwrap()) as usize;
            let Value::Map(mut entries) =
                ciborium::from_reader::<Value, _>(&block[56..56 + len]).expect("payload")
            else {
                panic!("map")
            };
            entries
                .iter_mut()
                .find(|(key, _)| *key == Value::Integer(5.into()))
                .expect("key 5")
                .1 = Value::Bool(true);
            if !keep_scheme {
                entries.retain(|(key, _)| *key != Value::Integer(1.into()));
            }
            entries.swap(0, 1);
            let mut bytes = Vec::new();
            ciborium::into_writer(&Value::Map(entries), &mut bytes).expect("encode");
            block[56..56 + len + 8].fill(0);
            block[44..48].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
            block[56..56 + bytes.len()].copy_from_slice(&bytes);
            let end = 56 + bytes.len();
            block[end..end + 8].copy_from_slice(&crate::crc64_xz(&bytes).to_le_bytes());
            let flags = if no_parity {
                crate::bootstrap::FLAG_NO_PARITY
            } else {
                0
            };
            block[12..16].copy_from_slice(&flags.to_be_bytes());
            let block = with_header_crc(block);
            assert!(matches!(
                parse_bootstrap_block(&block),
                Err(ParityError::BootstrapParse(_))
            ));
            block
        };
        // Parity tape: compression is refused even when the scheme also disagrees.
        let mut other_scheme = hints.clone();
        let crate::ParityConfig::Scheme(scheme) = &mut other_scheme.scheme else {
            panic!("parity hints")
        };
        scheme.data_blocks_per_stripe += 1;
        for supplied in [&hints, &other_scheme] {
            let error = supplied
                .classify_bootstrap(&build(false, true))
                .expect_err("recorded compression");
            assert!(matches!(error, ParityError::DriveCompressionEnabled { .. }));
            assert_eq!(error.to_string(), "tape's bootstrap records drive compression; a parity tape must not record drive compression");
        }
        // No-parity tape: recorded compression is not a refusal.
        let no_parity = ScanRecoveryHints {
            scheme: crate::ParityConfig::None,
            ..hints.clone()
        };
        assert!(matches!(
            no_parity.classify_bootstrap(&build(true, false)),
            Ok(RecoveryBootstrap::Unreadable(_))
        ));
        // A scheme record in the payload of a no-parity bootstrap is a value
        // that disagrees with the supplied no-parity scheme (Section 8.4).
        assert_eq!(
            refused_field(no_parity.classify_bootstrap(&build(true, true))),
            BootstrapRefusedField::Scheme
        );
    }

    /// REM-PARITY 8.2 and 8.4 with supplied values: a no-parity bootstrap that
    /// carries a scheme record is `BootstrapParse` on the parser, and discovery
    /// with supplied values refuses it, naming the scheme, whether the payload
    /// is canonical or not. `validate_bootstrap` refuses it the same way.
    #[test]
    fn no_parity_bootstrap_with_scheme_record_is_refused_with_supplied_values() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("map");
        let payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        let no_parity = ScanRecoveryHints {
            scheme: crate::ParityConfig::None,
            ..matching_hints(&payload)
        };
        // A well-formed, canonical parity payload under a no-parity header.
        let mut block = bootstrap_block_for_payload(&payload);
        block[12..16].copy_from_slice(&crate::bootstrap::FLAG_NO_PARITY.to_be_bytes());
        let block = with_header_crc(block);
        assert!(matches!(
            parse_bootstrap_block(&block),
            Err(ParityError::BootstrapParse(_))
        ));
        let refusal = no_parity.classify_bootstrap(&block);
        assert!(matches!(
            refusal,
            Err(ParityError::BootstrapRefused {
                field: BootstrapRefusedField::Scheme,
                ..
            })
        ));
        // The funnel also holds when a payload reaches validation directly.
        let mut direct = payload.clone();
        direct.no_parity_flag = true;
        assert_eq!(
            refused_field(no_parity.validate_bootstrap(&direct)),
            BootstrapRefusedField::Scheme
        );
        // A no-parity bootstrap without a scheme record is still readable.
        direct.scheme = None;
        assert!(no_parity.validate_bootstrap(&direct).is_ok());
    }

    /// BOT recovery reports only the UUID refusal as the operator's identity
    /// mismatch; every other refusal stays a scan failure with its wording.
    #[test]
    fn bot_recovery_maps_only_the_uuid_refusal_to_identity_mismatch() {
        let map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("map");
        let payload = bootstrap_payload(map.digest(false).expect("digest"), 0);
        let hints = matching_hints(&payload);
        let valid = bootstrap_block_for_payload(&payload);
        let recover = |first: Vec<u8>| {
            crate::bot_recovery::recover_terminal_inventory_from_bot_controlled_mode(
                &mut RecordingRawSource::new(vec![Record::Block(first), Record::Filemark]),
                &TAPE_UUID,
                BLOCK_SIZE,
                ScanMode::Recovery(&hints),
                |_| ScanWalkControl::Continue,
                |_| Ok(()),
            )
        };
        let mut foreign = valid.clone();
        foreign[16] ^= 1;
        foreign[80] ^= 1;
        assert!(matches!(
            recover(with_header_crc(foreign)).expect_err("foreign BOT"),
            crate::BotStructuralRecoveryError::TapeIdentityMismatch
        ));
        let mut sequence = valid.clone();
        sequence[36..44].copy_from_slice(&1u64.to_be_bytes());
        let short = valid[..valid.len() - 1].to_vec();
        for (first, wording) in [
            (with_header_crc(sequence), "got sequence 1"),
            (short, "short fixed-block bootstrap read"),
        ] {
            let error = recover(first).expect_err("refused BOT");
            assert!(
                matches!(&error, crate::BotStructuralRecoveryError::Scan { message } if message.contains(wording)),
                "{error}"
            );
        }
    }
}
