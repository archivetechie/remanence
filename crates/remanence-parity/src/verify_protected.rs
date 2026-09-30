//! A Verifier's full verification of protected content (REM-PARITY 2.2).
//!
//! The Verifier's validation is the Scanner's checks plus the Recoverer's
//! index and CRC validation, end to end, without recovering payload. For every
//! sidecar it therefore:
//!
//! - reads the primary copy, the tail copy and the footer, and reports damage
//!   to any of them, and a divergence between the copies, as `SidecarParse`
//!   even when the footer or the directory decides between them
//!   (Section 9.1);
//! - acquires the epoch's index through the Recoverer's own acquisition
//!   (Section 13.3), never a second path, and pins it as the Recoverer does;
//! - reads every data block that the sidecar protects and every parity shard,
//!   as opaque bytes, and checks each against the acquired index
//!   (Section 13.4), reporting each failure by the address Section 2.2 names:
//!   a data block's tape-file position, or a parity shard's epoch, stripe and
//!   parity index.
//!
//! A check of structure and metadata alone, which reads no data block or
//! parity shard, is not a full verification, and this module's result is the
//! only one that may report a tape's protected content as verified.

use crate::error::ParityError;
use crate::filemark_map::{ScopedFilemarkMap, TapeFileKind, TapeFileMapEntry, TapeFilePosition};
use crate::model::{ParityScheme, SidecarMetadataHealth};
use crate::raw::{
    read_fixed_record, tape_error_is_current_medium_damage, FixedRecordRead, PhysicalPositionHint,
    RawTapeSource,
};
use crate::recovery::{
    data_crc_for_ordinal, parity_crc_for, read_and_parse_sidecar_index, scheme_parity_blocks,
    sidecar_geometry_from_total, validate_sidecar_for_recovery,
};
use crate::sidecar::{
    data_shard_crc64, parity_block_position, parity_shard_crc64, parse_sidecar_footer_block,
    parse_sidecar_header_block, parse_sidecar_index_blocks, DecodedSidecarIndex, SidecarCopyKind,
    SidecarFooter,
};

/// Why a data block or a parity shard failed verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockFailureReason {
    /// The read failed with a medium error.
    Unreadable,
    /// The record was shorter or longer than one block (REM-PARITY 3.5), which
    /// is a read failure of the block.
    WrongLength {
        /// The record's measured length in bytes.
        measured_bytes: u64,
    },
    /// A filemark or end-of-data lay where the block should be.
    UnexpectedBoundary,
    /// The block read, but its CRC-64 disagrees with the acquired index.
    CrcMismatch,
}

impl BlockFailureReason {
    /// A fixed, stable description (never a debug rendering).
    pub fn describe(self) -> &'static str {
        match self {
            Self::Unreadable => "unreadable",
            Self::WrongLength { .. } => "record of the wrong length",
            Self::UnexpectedBoundary => "filemark or end of data where a block belongs",
            Self::CrcMismatch => "CRC mismatch",
        }
    }
}

/// A data block that a sidecar protects and that failed verification,
/// addressed by its tape-file position (REM-PARITY 2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedDataBlock {
    /// The block's tape-file position.
    pub position: TapeFilePosition,
    /// The block's `ParityDataOrdinal`.
    pub ordinal: u64,
    /// The block's physical position.
    pub physical: PhysicalPositionHint,
    /// Why it failed.
    pub reason: BlockFailureReason,
}

/// A parity shard that failed verification, addressed by its epoch, stripe and
/// parity index (REM-PARITY 2.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedParityShard {
    /// The epoch the shard belongs to.
    pub epoch_id: u64,
    /// The shard's stripe within the epoch.
    pub stripe_index: u32,
    /// The shard's parity index.
    pub parity_index: u16,
    /// The tape-file number of the sidecar that holds the shard.
    pub sidecar_tape_file_number: u64,
    /// The shard's block within the sidecar tape file.
    pub sidecar_block: u64,
    /// The shard's physical position.
    pub physical: PhysicalPositionHint,
    /// Why it failed.
    pub reason: BlockFailureReason,
}

/// What reading one sidecar component (a copy or the footer) found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarComponentState {
    /// A block of it could not be read (medium error): nothing can be said of
    /// its content, and a Reader records copy health only.
    Unreadable,
    /// It read, and its content violates Section 9 (or, for a footer, its
    /// total disagrees with the map entry).
    Invalid {
        /// The violation.
        detail: String,
    },
    /// It validates on its own.
    Valid,
    /// Nothing located it: no valid footer or primary copy and no consistent
    /// geometry told where it lies.
    NotLocated,
}

/// The Section 15 error a sidecar finding is reported as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidecarFindingKind {
    /// Sidecar structure violates Section 9: an invalid copy or footer, or a
    /// divergence between the copies.
    SidecarParse,
    /// No header/index copy validated (Section 13.3): only this epoch.
    SidecarMetadataUnavailable,
    /// The acquired index disagrees with the bootstrap's (or the supplied)
    /// scheme or with the map entry's range.
    SchemeMismatch,
    /// A footer or copy could not be read, or nothing located it. This is
    /// damage a Verifier reports (REM-PARITY 2.2) although a Reader names no
    /// error for it: only copy health is recorded. Never `SidecarParse`.
    CopyHealth,
}

impl SidecarFindingKind {
    /// The Section 15 name.
    pub fn section_15_name(self) -> &'static str {
        match self {
            Self::SidecarParse => "SidecarParse",
            Self::SidecarMetadataUnavailable => "SidecarMetadataUnavailable",
            Self::SchemeMismatch => "SchemeMismatch",
            Self::CopyHealth => "(copy health; no Section 15 name)",
        }
    }
}

/// One finding about a sidecar's metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarFinding {
    /// The error a Reader reports for it.
    pub kind: SidecarFindingKind,
    /// What was found.
    pub detail: String,
}

/// The full verification of one sidecar and the data and parity it protects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidecarVerification {
    /// The sidecar's tape-file number.
    pub tape_file_number: u64,
    /// The epoch the map entry names.
    pub epoch_id: u64,
    /// The footer, read from the file's last block.
    pub footer: SidecarComponentState,
    /// The primary header/index copy.
    pub primary: SidecarComponentState,
    /// The tail header/index copy.
    pub tail: SidecarComponentState,
    /// Both copies validate on their own and differ (Section 9.1).
    pub copies_diverge: bool,
    /// Copy health of the index the Recoverer's acquisition used, or `None`
    /// when no copy could be used.
    pub metadata_health: Option<SidecarMetadataHealth>,
    /// Whether every block and shard was checked against an acquired, pinned
    /// index. When not, only read failures could be found.
    pub checked_against_index: bool,
    /// Findings about the sidecar's metadata, each with its Section 15 name.
    pub findings: Vec<SidecarFinding>,
    /// Data blocks that this sidecar protects and that failed.
    pub failed_data_blocks: Vec<FailedDataBlock>,
    /// Parity shards of this sidecar that failed.
    pub failed_parity_shards: Vec<FailedParityShard>,
}

impl SidecarVerification {
    /// Whether the sidecar verified with no finding and no failed block or shard.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
            && self.failed_data_blocks.is_empty()
            && self.failed_parity_shards.is_empty()
            && self.checked_against_index
    }

    /// Whether any finding is of `kind`.
    pub fn has_finding(&self, kind: SidecarFindingKind) -> bool {
        self.findings.iter().any(|finding| finding.kind == kind)
    }
}

/// A ParityMap copy that could not be used, found by a full verification
/// (REM-PARITY 2.2). A copy or footer that is not used has no Section 15
/// name: a Reader that uses the other copy reports no error, and the Verifier
/// still reports the damage. When no copy is usable the detail carries the
/// name the Reader reports (`ParityMapParse` or `DirectoryInvalid`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityMapFinding {
    /// The ParityMap's tape-file number.
    pub tape_file_number: u64,
    /// What was found.
    pub detail: String,
}

/// The full verification of every sidecar's protected content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtectedContentVerification {
    /// One entry per sidecar, in tape-file order.
    pub sidecars: Vec<SidecarVerification>,
    /// ParityMap copies that were unreadable or invalid, reported even when
    /// the other copy is used; a tape with one is not complete.
    pub parity_map_findings: Vec<ParityMapFinding>,
    /// Why the pass could not be performed, when it was not. A pass that was
    /// not performed is never clean.
    pub not_performed: Option<String>,
    /// Damage or nonconformity (including bootstrap trailing fill) found inside
    /// the pre-tail prefix, retained even when the protected pass cannot run.
    pub prefix_damage: Vec<String>,
}

/// Report fill evidence from the scanner's existing bootstrap parse.
pub(crate) fn bootstrap_fill_finding(
    candidate: &crate::scan::ScanBootstrapCandidate,
) -> Option<String> {
    candidate.nonzero_fill.then(|| {
        format!(
            "bootstrap at tape_file {} has nonzero trailing fill (REM-PARITY 8.1)",
            candidate.tape_file_number
        )
    })
}

impl ProtectedContentVerification {
    /// A tape with no parity protects no content: nothing to verify.
    pub fn none_protected() -> Self {
        Self {
            sidecars: Vec::new(),
            parity_map_findings: Vec::new(),
            not_performed: None,
            prefix_damage: Vec::new(),
        }
    }

    /// A pass that could not be performed, with the reason.
    pub fn not_performed(reason: impl Into<String>) -> Self {
        Self {
            sidecars: Vec::new(),
            parity_map_findings: Vec::new(),
            not_performed: Some(reason.into()),
            prefix_damage: Vec::new(),
        }
    }

    /// Whether the pass was performed and found nothing wrong.
    pub fn is_clean(&self) -> bool {
        self.not_performed.is_none()
            && self.prefix_damage.is_empty()
            && self.parity_map_findings.is_empty()
            && self.sidecars.iter().all(SidecarVerification::is_clean)
    }

    /// Every data block that failed, over all sidecars.
    pub fn failed_data_blocks(&self) -> impl Iterator<Item = &FailedDataBlock> {
        self.sidecars
            .iter()
            .flat_map(|sidecar| sidecar.failed_data_blocks.iter())
    }

    /// Every parity shard that failed, over all sidecars.
    pub fn failed_parity_shards(&self) -> impl Iterator<Item = &FailedParityShard> {
        self.sidecars
            .iter()
            .flat_map(|sidecar| sidecar.failed_parity_shards.iter())
    }
}

/// What a probe of one block found.
enum Probe {
    Block(Vec<u8>),
    /// The read failed with a medium error.
    Unreadable,
    /// A record shorter or longer than one block (REM-PARITY 3.5).
    WrongLength(u64),
    /// A filemark or end-of-data where a block belongs.
    Boundary(&'static str),
}

impl Probe {
    /// Why a record that is not a block failed, for a data block or a shard.
    fn failure(&self) -> Option<BlockFailureReason> {
        match self {
            Self::Block(_) => None,
            Self::Unreadable => Some(BlockFailureReason::Unreadable),
            Self::WrongLength(measured_bytes) => Some(BlockFailureReason::WrongLength {
                measured_bytes: *measured_bytes,
            }),
            Self::Boundary(_) => Some(BlockFailureReason::UnexpectedBoundary),
        }
    }

    /// What a record that is not a block says, for a sidecar copy or footer.
    fn detail(&self, block_size: u32) -> String {
        match self {
            Self::WrongLength(measured_bytes) => {
                format!("record of {measured_bytes} bytes, not one {block_size}-byte block")
            }
            Self::Boundary(what) => (*what).to_string(),
            Self::Block(_) | Self::Unreadable => String::new(),
        }
    }
}

/// Reads blocks in order, locating only when the next block is not where the
/// last read left the drive, so a whole-tape pass streams.
struct SequentialReader<'a> {
    source: &'a mut dyn RawTapeSource,
    block_size: u32,
    next: Option<PhysicalPositionHint>,
}

impl<'a> SequentialReader<'a> {
    fn new(source: &'a mut dyn RawTapeSource, block_size: u32) -> Self {
        Self {
            source,
            block_size,
            next: None,
        }
    }

    fn read(&mut self, physical: PhysicalPositionHint) -> Result<Probe, ParityError> {
        if self.next != Some(physical) {
            self.next = None;
            self.source.locate_physical(physical)?;
        }
        let mut block = vec![0u8; self.block_size as usize];
        match read_fixed_record(self.source, &mut block) {
            Ok(FixedRecordRead::Block { position_after }) => {
                self.next = Some(position_after);
                Ok(Probe::Block(block))
            }
            Ok(FixedRecordRead::WrongLength { measured_bytes }) => {
                self.next = None;
                Ok(Probe::WrongLength(measured_bytes))
            }
            Ok(FixedRecordRead::Filemark { .. }) => {
                self.next = None;
                Ok(Probe::Boundary("filemark where a block belongs"))
            }
            Ok(FixedRecordRead::EndOfData { .. }) => {
                self.next = None;
                Ok(Probe::Boundary("end-of-data where a block belongs"))
            }
            Err(ParityError::TapeIo(error)) if tape_error_is_current_medium_damage(&error) => {
                self.next = None;
                Ok(Probe::Unreadable)
            }
            // A device fault that is not medium damage is the device's, not a
            // fact about the block (REM-PARITY 15).
            Err(error) => Err(error),
        }
    }

    fn read_sidecar_block(
        &mut self,
        scoped_map: &ScopedFilemarkMap,
        sidecar: &TapeFileMapEntry,
        block_within_file: u64,
    ) -> Result<Probe, ParityError> {
        let physical = scoped_map.map.physical_position(TapeFilePosition {
            tape_file_number: sidecar.tape_file_number,
            block_within_file,
        })?;
        self.read(physical)
    }
}

/// One header/index copy as read: its blocks parsed on their own.
enum CopyProbe {
    Unreadable,
    Invalid(String),
    Valid(Box<DecodedSidecarIndex>),
    /// No locator placed the copy.
    NotLocated,
}

/// Read and validate one header/index copy starting at `start` (Sections 9.2
/// to 9.5), and check its copy kind and that its total matches the map entry.
fn read_copy(
    reader: &mut SequentialReader<'_>,
    scoped_map: &ScopedFilemarkMap,
    sidecar: &TapeFileMapEntry,
    tape_uuid: &[u8; 16],
    start: u64,
    kind: SidecarCopyKind,
) -> Result<CopyProbe, ParityError> {
    let block0 = match reader.read_sidecar_block(scoped_map, sidecar, start)? {
        Probe::Block(block) => block,
        Probe::Unreadable => return Ok(CopyProbe::Unreadable),
        other => return Ok(CopyProbe::Invalid(other.detail(reader.block_size))),
    };
    let header = match parse_sidecar_header_block(&block0, tape_uuid) {
        Ok(header) => header,
        Err(error) => return Ok(CopyProbe::Invalid(error.to_string())),
    };
    let mut blocks = vec![block0];
    // No capacity is reserved from a count the copy itself states.
    for offset in 1..header.shard_index_block_count {
        let Some(block_within_file) = start.checked_add(offset) else {
            return Ok(CopyProbe::Invalid(
                "index block offset overflows u64".into(),
            ));
        };
        if block_within_file >= sidecar.block_count {
            return Ok(CopyProbe::Invalid(
                "index blocks run past the sidecar tape file".into(),
            ));
        }
        match reader.read_sidecar_block(scoped_map, sidecar, block_within_file)? {
            Probe::Block(block) => blocks.push(block),
            Probe::Unreadable => return Ok(CopyProbe::Unreadable),
            other => return Ok(CopyProbe::Invalid(other.detail(reader.block_size))),
        }
    }
    let decoded = match parse_sidecar_index_blocks(&blocks, tape_uuid) {
        Ok(decoded) => decoded,
        Err(error) => return Ok(CopyProbe::Invalid(error.to_string())),
    };
    if decoded.header.copy_kind != kind {
        return Ok(CopyProbe::Invalid(format!(
            "{kind:?} copy decoded as {:?}",
            decoded.header.copy_kind
        )));
    }
    if decoded.header.sidecar_total_block_count != sidecar.block_count {
        return Ok(CopyProbe::Invalid(format!(
            "copy total {} disagrees with the map entry's {}",
            decoded.header.sidecar_total_block_count, sidecar.block_count
        )));
    }
    Ok(CopyProbe::Valid(Box::new(decoded)))
}

fn component_state(probe: &CopyProbe) -> SidecarComponentState {
    match probe {
        CopyProbe::Unreadable => SidecarComponentState::Unreadable,
        CopyProbe::Invalid(detail) => SidecarComponentState::Invalid {
            detail: detail.clone(),
        },
        CopyProbe::Valid(_) => SidecarComponentState::Valid,
        CopyProbe::NotLocated => SidecarComponentState::NotLocated,
    }
}

/// Where the tail copy lies, from the best available locator: a valid footer,
/// the primary's header, or the geometry the scheme and the map entry give
/// (`H = (total - 1 - P) / 2`, REM-PARITY 13.3).
fn tail_start(
    footer: Option<&SidecarFooter>,
    primary: Option<&DecodedSidecarIndex>,
    sidecar: &TapeFileMapEntry,
    scheme: &ParityScheme,
) -> Option<u64> {
    if let Some(footer) = footer {
        return Some(footer.tail_header_start_block);
    }
    if let Some(primary) = primary {
        return Some(primary.header.tail_header_start_block);
    }
    sidecar_geometry_from_total(sidecar.block_count, scheme_parity_blocks(scheme))
        .map(|(_, tail_start)| tail_start)
}

/// Verify one sidecar in full (REM-PARITY 2.2, 9.1, 13.3, 13.4).
///
/// `scheme` is the bootstrap's scheme record, or the supplied scheme when the
/// bootstrap is unreadable. The index is acquired by the Recoverer's own
/// acquisition and pinned exactly as the Recoverer pins it.
pub fn verify_sidecar(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    sidecar: &TapeFileMapEntry,
    scheme: &ParityScheme,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<SidecarVerification, ParityError> {
    let (start, end, epoch_id) = match (
        sidecar.protected_ordinal_start,
        sidecar.protected_ordinal_end_exclusive,
        sidecar.epoch_id,
    ) {
        (Some(start), Some(end), Some(epoch_id)) if start < end => (start, end, epoch_id),
        _ => {
            return Err(ParityError::FilemarkMapReconstruct(format!(
                "parity sidecar tape file {} has incomplete epoch range metadata",
                sidecar.tape_file_number
            )))
        }
    };
    source.configure_fixed_block_size(block_size)?;
    let mut findings = Vec::new();
    let mut reader = SequentialReader::new(source, block_size);

    // The footer, the primary copy and the tail copy, each on its own.
    let footer_probe = if sidecar.block_count == 0 {
        Probe::Boundary("empty sidecar tape file")
    } else {
        reader.read_sidecar_block(scoped_map, sidecar, sidecar.block_count - 1)?
    };
    let (footer_state, footer) = match footer_probe {
        Probe::Unreadable => (SidecarComponentState::Unreadable, None),
        Probe::Block(block) => match parse_sidecar_footer_block(&block, tape_uuid) {
            Err(error) => (
                SidecarComponentState::Invalid {
                    detail: error.to_string(),
                },
                None,
            ),
            Ok(footer) if footer.sidecar_total_block_count != sidecar.block_count => (
                SidecarComponentState::Invalid {
                    detail: format!(
                        "footer total {} disagrees with the measured {}",
                        footer.sidecar_total_block_count, sidecar.block_count
                    ),
                },
                None,
            ),
            Ok(footer) => (SidecarComponentState::Valid, Some(footer)),
        },
        other => (
            SidecarComponentState::Invalid {
                detail: other.detail(block_size),
            },
            None,
        ),
    };
    // An empty sidecar file has no block to probe: its copies are not located.
    let primary = if sidecar.block_count == 0 {
        CopyProbe::NotLocated
    } else {
        read_copy(
            &mut reader,
            scoped_map,
            sidecar,
            tape_uuid,
            0,
            SidecarCopyKind::Primary,
        )?
    };
    let primary_index = match &primary {
        CopyProbe::Valid(index) => Some(index.as_ref()),
        _ => None,
    };
    let tail = match tail_start(footer.as_ref(), primary_index, sidecar, scheme) {
        Some(tail_start) if tail_start < sidecar.block_count => read_copy(
            &mut reader,
            scoped_map,
            sidecar,
            tape_uuid,
            tail_start,
            SidecarCopyKind::Tail,
        )?,
        Some(_) => CopyProbe::Invalid("the tail locator lies outside the file".into()),
        None => CopyProbe::NotLocated,
    };
    let tail_state = component_state(&tail);
    for (what, state) in [
        ("footer", &footer_state),
        ("primary copy", &component_state(&primary)),
        ("tail copy", &tail_state),
    ] {
        if let SidecarComponentState::Invalid { detail } = state {
            findings.push(SidecarFinding {
                kind: SidecarFindingKind::SidecarParse,
                detail: format!("{what}: {detail}"),
            });
        }
    }
    // A footer or copy that could not be read, or that nothing located, is
    // damage the Verifier reports, though it is no `SidecarParse`: a lost
    // metadata copy is lost redundancy.
    for (what, state) in [
        ("footer", &footer_state),
        ("primary copy", &component_state(&primary)),
        ("tail copy", &tail_state),
    ] {
        let detail = match state {
            SidecarComponentState::Unreadable => "could not be read (medium error)",
            SidecarComponentState::NotLocated => "was not located by any locator",
            _ => continue,
        };
        findings.push(SidecarFinding {
            kind: SidecarFindingKind::CopyHealth,
            detail: format!("{what} {detail}"),
        });
    }
    // A copy that validates on its own but that a valid footer contradicts.
    if let Some(footer) = &footer {
        for (what, probe) in [("primary copy", &primary), ("tail copy", &tail)] {
            if let CopyProbe::Valid(copy) = probe {
                if copy.header.canonical_metadata_hash != footer.canonical_metadata_hash {
                    findings.push(SidecarFinding {
                        kind: SidecarFindingKind::SidecarParse,
                        detail: format!("{what} contradicts the footer's canonical metadata hash"),
                    });
                }
            }
        }
    }
    // Both copies validate and differ: divergence (Section 9.1), reported even
    // when the footer or the directory decides between them.
    let copies_diverge = match (&primary, &tail) {
        (CopyProbe::Valid(primary), CopyProbe::Valid(tail)) => {
            primary.header.canonical_metadata_hash != tail.header.canonical_metadata_hash
                || primary.index != tail.index
        }
        _ => false,
    };
    if copies_diverge {
        findings.push(SidecarFinding {
            kind: SidecarFindingKind::SidecarParse,
            detail: "the primary and tail copies both validate and differ".into(),
        });
    }

    // The index, acquired as the Recoverer acquires it.
    let acquired = match read_and_parse_sidecar_index(
        reader.source,
        scoped_map,
        sidecar,
        scheme,
        tape_uuid,
        block_size,
    ) {
        Ok(read) => Some(read),
        Err(error @ ParityError::SidecarMetadataUnavailable { .. }) => {
            findings.push(SidecarFinding {
                kind: SidecarFindingKind::SidecarMetadataUnavailable,
                detail: error.to_string(),
            });
            None
        }
        Err(error) => return Err(error),
    };
    reader.next = None;
    let metadata_health = acquired.as_ref().map(|read| read.metadata_health);
    // The Recoverer's pin against the scheme and the map entry's range.
    let usable = match &acquired {
        Some(read) => {
            match validate_sidecar_for_recovery(&read.index, sidecar, scheme, epoch_id, block_size)
            {
                Ok(()) => true,
                Err(error) => {
                    findings.push(SidecarFinding {
                        kind: if matches!(error, ParityError::SchemeMismatch { .. }) {
                            SidecarFindingKind::SchemeMismatch
                        } else {
                            SidecarFindingKind::SidecarParse
                        },
                        detail: error.to_string(),
                    });
                    false
                }
            }
        }
        None => false,
    };
    let index = acquired.as_ref().map(|read| &read.index);

    // Every data block the sidecar protects, in physical order.
    let mut failed_data_blocks = Vec::new();
    for entry in scoped_map.map.entries() {
        if entry.kind != TapeFileKind::Object {
            continue;
        }
        let Some(first) = entry.first_parity_data_ordinal else {
            continue;
        };
        let Some(object_end) = first.checked_add(entry.block_count) else {
            return Err(ParityError::FilemarkMapReconstruct(format!(
                "object tape file {} ordinal range overflows",
                entry.tape_file_number
            )));
        };
        let overlap_start = first.max(start);
        let overlap_end = object_end.min(end);
        if overlap_start >= overlap_end {
            continue;
        }
        let mut ordinal = overlap_start;
        while ordinal < overlap_end {
            let position = TapeFilePosition {
                tape_file_number: entry.tape_file_number,
                block_within_file: ordinal - first,
            };
            let physical = scoped_map.map.physical_position(position)?;
            let probe = reader.read(physical)?;
            let reason = match &probe {
                Probe::Block(block) => match index.filter(|_| usable) {
                    // The Recoverer's own data CRC lookup and comparison.
                    Some(index) => (data_crc_for_ordinal(index, ordinal, start).ok()
                        != Some(data_shard_crc64(block)))
                    .then_some(BlockFailureReason::CrcMismatch),
                    None => None,
                },
                failed => failed.failure(),
            };
            if let Some(reason) = reason {
                failed_data_blocks.push(FailedDataBlock {
                    position,
                    ordinal,
                    physical,
                    reason,
                });
            }
            ordinal += 1;
        }
    }

    // Every parity shard, in physical (parity-index-major) order.
    let mut failed_parity_shards = Vec::new();
    let geometry = match index {
        Some(index) => Some((
            index.header.shard_index_block_count,
            index.header.stripes_per_epoch,
            index.header.m,
        )),
        None => {
            // No index: the geometry the scheme and the map entry imply.
            sidecar_geometry_from_total(sidecar.block_count, scheme_parity_blocks(scheme)).map(
                |(h, _)| {
                    (
                        h,
                        scheme.stripes_per_neighborhood,
                        scheme.parity_blocks_per_stripe,
                    )
                },
            )
        }
    };
    if let Some((header_blocks, stripes, parity_per_stripe)) = geometry {
        for parity_index in 0..parity_per_stripe {
            for stripe_index in 0..stripes {
                let sidecar_block = parity_block_position(
                    stripe_index,
                    parity_index,
                    stripes,
                    parity_per_stripe,
                    header_blocks,
                )?;
                if sidecar_block >= sidecar.block_count {
                    break;
                }
                let physical = scoped_map.map.physical_position(TapeFilePosition {
                    tape_file_number: sidecar.tape_file_number,
                    block_within_file: sidecar_block,
                })?;
                let probe = reader.read(physical)?;
                let reason = match &probe {
                    Probe::Block(block) => match index.filter(|_| usable) {
                        // The Recoverer's own parity entry lookup.
                        Some(index) => (parity_crc_for(index, stripe_index, parity_index).ok()
                            != Some(parity_shard_crc64(block)))
                        .then_some(BlockFailureReason::CrcMismatch),
                        None => None,
                    },
                    failed => failed.failure(),
                };
                if let Some(reason) = reason {
                    failed_parity_shards.push(FailedParityShard {
                        epoch_id,
                        stripe_index,
                        parity_index,
                        sidecar_tape_file_number: sidecar.tape_file_number,
                        sidecar_block,
                        physical,
                        reason,
                    });
                }
            }
        }
    }

    Ok(SidecarVerification {
        tape_file_number: sidecar.tape_file_number,
        epoch_id,
        footer: footer_state,
        primary: component_state(&primary),
        tail: tail_state,
        copies_diverge,
        metadata_health,
        checked_against_index: usable,
        findings,
        failed_data_blocks,
        failed_parity_shards,
    })
}

/// Verify every sidecar of a validated map in full (REM-PARITY 2.2).
///
/// Only sidecars inside the map's validated prefix are verified: the durable
/// boundary bounds what the Recoverer may trust, and the Verifier does not
/// read past it.
pub fn verify_protected_content(
    source: &mut dyn RawTapeSource,
    scoped_map: &ScopedFilemarkMap,
    scheme: &ParityScheme,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<ProtectedContentVerification, ParityError> {
    let mut sidecars = Vec::new();
    let mut parity_map_findings = Vec::new();
    for entry in scoped_map.map.entries() {
        if entry.kind == TapeFileKind::ParityMap && scoped_map.is_validated(entry.tape_file_number)
        {
            // Both copies of every ParityMap are read, and one that cannot be
            // used is reported even when the other is (REM-PARITY 2.2).
            let blocks = crate::parity_map::read_parity_map_blocks(
                source,
                &scoped_map.map,
                entry,
                block_size,
            )?;
            for detail in crate::parity_map::parity_map_damage(&blocks, tape_uuid) {
                parity_map_findings.push(ParityMapFinding {
                    tape_file_number: entry.tape_file_number,
                    detail,
                });
            }
            continue;
        }
        if entry.kind != TapeFileKind::ParitySidecar
            || !scoped_map.is_validated(entry.tape_file_number)
        {
            continue;
        }
        sidecars.push(verify_sidecar(
            source, scoped_map, entry, scheme, tape_uuid, block_size,
        )?);
    }
    Ok(ProtectedContentVerification {
        sidecars,
        parity_map_findings,
        not_performed: None,
        prefix_damage: Vec::new(),
    })
}
