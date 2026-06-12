//! Catalog-less filemark-map reconstruction for Layer 3c v0.4.4.
//!
//! The scanner walks physical tape files from BOT, reads only the first block
//! of each file for structural classification, and measures file length by
//! spacing to the next filemark. Bootstrap and sidecar tape files are accepted
//! only after their magic plus CRC/header validation succeeds; everything else
//! is treated as an object candidate and validated later by the bootstrap
//! filemark-map digest.

use crate::bootstrap::{has_bootstrap_magic, parse_bootstrap_block, BootstrapPayload};
use crate::error::ParityError;
use crate::filemark_map::{
    FilemarkMap, FilemarkMapBuilder, ScopedFilemarkMap, TapeFileKind, TapeFileMapEntry,
    TapeFilePosition,
};
use crate::parity_map::{
    classify_parity_map_header_block, parse_parity_map_tape_file, ParityMapReference,
    SidecarEpochDirectory,
};
use crate::raw::{PhysicalPositionHint, RawReadOutcome, RawTapeSource};
use crate::sidecar::{
    classify_sidecar_header_block, parse_sidecar_footer_block, parse_sidecar_index_blocks,
    SidecarFooter, SidecarHeader,
};

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
/// `filemark_map_digest`, preserving intermediate-bootstrap prefix scope.
pub fn acquire_filemark_map(
    source: &mut dyn RawTapeSource,
    authoritative_bootstrap: &BootstrapPayload,
    catalog_map: Option<CatalogFilemarkMapInput>,
) -> Result<ScopedFilemarkMap, ParityError> {
    if !authoritative_bootstrap.no_parity_flag && authoritative_bootstrap.drive_compression {
        return Err(ParityError::DriveCompressionEnabled);
    }

    if let Some(catalog) = catalog_map {
        validate_catalog_scope(&catalog, authoritative_bootstrap)?;
        return Ok(ScopedFilemarkMap::from_catalog(
            catalog.map,
            catalog.highest_protected_ordinal,
        ));
    }

    let Some(digest) = authoritative_bootstrap.filemark_map_digest.as_ref() else {
        return Err(filemark_scan_error(
            "authoritative bootstrap does not carry a filemark-map digest",
        ));
    };
    let reconstructed = scan_reconstruct_filemark_map(
        source,
        &authoritative_bootstrap.tape_uuid,
        authoritative_bootstrap.block_size_bytes,
    )?;
    let reconstructed =
        apply_authoritative_directory_overlay(source, reconstructed, authoritative_bootstrap)?;
    ScopedFilemarkMap::validate_against_digest(reconstructed, digest)
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

    loop {
        let file_start = source.position()?;
        match source.read_record(&mut buf) {
            Ok(RawReadOutcome::EndOfData { .. }) => break,
            Ok(RawReadOutcome::Filemark { .. }) => {
                return Err(filemark_scan_error(format!(
                    "empty tape file at physical LBA {}",
                    file_start.lba
                )));
            }
            Ok(RawReadOutcome::Block { bytes, .. }) if bytes != block_size_usize => {
                return Err(filemark_scan_error(format!(
                    "short fixed-block scan read at physical LBA {}: got {bytes}, expected {block_size_usize}",
                    file_start.lba
                )));
            }
            Ok(RawReadOutcome::Block { .. }) => {
                let first_block = buf.clone();
                let measured = measure_current_file(source, file_start)?;
                append_classified_entry(
                    source,
                    &mut builder,
                    &first_block,
                    tape_uuid,
                    block_size,
                    file_start,
                    measured.block_count,
                )?;
                source.locate_physical(measured.position_after)?;
                saw_file = true;
            }
            Err(_err) => {
                source.locate_physical(file_start)?;
                let measured = measure_current_file(source, file_start)?;
                append_entry_with_unreadable_head(
                    source,
                    &mut builder,
                    tape_uuid,
                    block_size,
                    file_start,
                    measured.block_count,
                )?;
                source.locate_physical(measured.position_after)?;
                saw_file = true;
            }
        }
    }

    if !saw_file {
        return Err(filemark_scan_error("scan found no tape files"));
    }

    builder.build()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MeasuredTapeFile {
    block_count: u64,
    position_after: PhysicalPositionHint,
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
) -> Result<MeasuredTapeFile, ParityError> {
    let outcome = source.space_filemarks(1)?;
    if outcome.filemarks_spaced != 1 {
        return Err(filemark_scan_error(format!(
            "tape file at physical LBA {} is missing a trailing filemark",
            file_start.lba
        )));
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
        return Err(filemark_scan_error(format!(
            "tape file at physical LBA {} has no data blocks",
            file_start.lba
        )));
    }
    Ok(MeasuredTapeFile {
        block_count,
        position_after: outcome.position_after,
    })
}

fn append_classified_entry(
    source: &mut dyn RawTapeSource,
    builder: &mut FilemarkMapBuilder,
    block0: &[u8],
    tape_uuid: &[u8; 16],
    block_size: u32,
    file_start: PhysicalPositionHint,
    block_count: u64,
) -> Result<(), ParityError> {
    if has_bootstrap_magic(block0) {
        match parse_bootstrap_block(block0) {
            Ok(payload) => {
                if payload.block_size_bytes == block_size {
                    if block_count != 1 {
                        return Err(filemark_scan_error(format!(
                            "bootstrap tape file has block_count {block_count}, expected 1"
                        )));
                    }
                    builder.push_bootstrap()?;
                    return Ok(());
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
            return Err(filemark_scan_error(format!(
                "parity-map sequence {} has block_count {block_count}, expected {expected}",
                header.sequence
            )));
        }
        builder.push_parity_map(block_count)?;
        return Ok(());
    }

    if let Ok(Some(header)) = classify_sidecar_header_block(block0, tape_uuid) {
        let expected = header.sidecar_total_block_count;
        if block_count != expected {
            return Err(filemark_scan_error(format!(
                "sidecar epoch {} has block_count {block_count}, expected {expected}",
                header.epoch_id
            )));
        }
        builder.push_parity_sidecar(
            block_count,
            header.epoch_id,
            header.protected_ordinal_start,
            header.protected_ordinal_end_exclusive,
        )?;
        return Ok(());
    }

    if let Some(header) =
        classify_sidecar_from_footer_tail(source, file_start, tape_uuid, block_size, block_count)?
    {
        builder.push_parity_sidecar(
            block_count,
            header.epoch_id,
            header.protected_ordinal_start,
            header.protected_ordinal_end_exclusive,
        )?;
        return Ok(());
    }

    builder.push_object(block_count)?;
    Ok(())
}

fn append_entry_with_unreadable_head(
    source: &mut dyn RawTapeSource,
    builder: &mut FilemarkMapBuilder,
    tape_uuid: &[u8; 16],
    block_size: u32,
    file_start: PhysicalPositionHint,
    block_count: u64,
) -> Result<(), ParityError> {
    if let Some(header) =
        classify_sidecar_from_footer_tail(source, file_start, tape_uuid, block_size, block_count)?
    {
        builder.push_parity_sidecar(
            block_count,
            header.epoch_id,
            header.protected_ordinal_start,
            header.protected_ordinal_end_exclusive,
        )?;
    } else {
        builder.push_object(block_count)?;
    }
    Ok(())
}

fn classify_sidecar_from_footer_tail(
    source: &mut dyn RawTapeSource,
    file_start: PhysicalPositionHint,
    tape_uuid: &[u8; 16],
    block_size: u32,
    block_count: u64,
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
        return Err(filemark_scan_error(format!(
            "sidecar footer epoch {} has block_count {block_count}, expected {}",
            footer.epoch_id, footer.sidecar_total_block_count
        )));
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
    for offset in 0..u64::from(footer.sidecar_header_block_count) {
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
        | Ok(RawReadOutcome::EndOfData { .. })
        | Err(_) => Ok(None),
    }
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

fn apply_authoritative_directory_overlay(
    source: &mut dyn RawTapeSource,
    reconstructed: FilemarkMap,
    authoritative_bootstrap: &BootstrapPayload,
) -> Result<FilemarkMap, ParityError> {
    if let Some(directory) = authoritative_bootstrap.sidecar_epoch_directory.as_ref() {
        return apply_sidecar_directory_overlay(reconstructed, directory, None);
    }

    if let Some(reference) = authoritative_bootstrap.parity_map_reference.as_ref() {
        if let Some(directory) = read_referenced_parity_map_directory(
            source,
            &reconstructed,
            reference,
            &authoritative_bootstrap.tape_uuid,
            authoritative_bootstrap.block_size_bytes,
        )? {
            return apply_sidecar_directory_overlay(reconstructed, &directory, Some(reference));
        }
        return apply_parity_map_reference_structural_overlay(reconstructed, reference);
    }

    Ok(reconstructed)
}

fn read_referenced_parity_map_directory(
    source: &mut dyn RawTapeSource,
    reconstructed: &FilemarkMap,
    reference: &ParityMapReference,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<Option<SidecarEpochDirectory>, ParityError> {
    let reference_index = usize::try_from(reference.tape_file_number).map_err(|_| {
        filemark_scan_error(format!(
            "parity_map reference tape file {} does not fit usize",
            reference.tape_file_number
        ))
    })?;
    let Some(entry) = reconstructed.entries().get(reference_index) else {
        return Ok(None);
    };
    if entry.tape_file_number != reference.tape_file_number
        || entry.block_count != reference.block_count
    {
        return Ok(None);
    }

    let block_capacity = usize::try_from(reference.block_count).map_err(|_| {
        filemark_scan_error(format!(
            "parity_map reference block_count {} does not fit usize",
            reference.block_count
        ))
    })?;
    let mut blocks = Vec::with_capacity(block_capacity);
    for block_within_file in 0..reference.block_count {
        let position = reconstructed.physical_position(TapeFilePosition {
            tape_file_number: reference.tape_file_number,
            block_within_file,
        })?;
        source.locate_physical(position)?;
        let Some(block) = read_fixed_block_at_current_position(source, block_size)? else {
            return Ok(None);
        };
        blocks.push(block);
    }

    let decoded = match parse_parity_map_tape_file(&blocks, tape_uuid) {
        Ok(decoded) => decoded,
        Err(_) => return Ok(None),
    };
    if decoded.header.payload_sha256 != reference.parity_map_payload_sha256
        || decoded.payload.canonical_map_digest != reference.canonical_map_digest
        || decoded.payload.directory.directory_scope_tape_file_count
            != reference.directory_scope_tape_file_count
        || decoded
            .payload
            .directory
            .directory_scope_total_data_ordinals
            != reference.directory_scope_total_data_ordinals
        || decoded
            .payload
            .directory
            .directory_scope_highest_protected_ordinal
            != reference.directory_scope_highest_protected_ordinal
        || decoded.payload.directory.is_final_directory != reference.is_final_directory
    {
        return Ok(None);
    }

    Ok(Some(decoded.payload.directory))
}

fn apply_parity_map_reference_structural_overlay(
    reconstructed: FilemarkMap,
    reference: &ParityMapReference,
) -> Result<FilemarkMap, ParityError> {
    let mut found_reference = false;
    let mut next_object_ordinal = 0u64;
    let mut overlayed_entries = Vec::with_capacity(reconstructed.entries().len());

    for entry in reconstructed.entries() {
        if entry.tape_file_number == reference.tape_file_number {
            found_reference = true;
            if entry.block_count != reference.block_count {
                return Err(filemark_scan_error(format!(
                    "parity_map reference {} has block_count {}, scanned {}",
                    reference.tape_file_number, reference.block_count, entry.block_count
                )));
            }
            if !matches!(entry.kind, TapeFileKind::Object | TapeFileKind::ParityMap) {
                return Err(filemark_scan_error(format!(
                    "parity_map reference {} conflicts with scanned {:?} tape file",
                    reference.tape_file_number, entry.kind
                )));
            }
            overlayed_entries.push(TapeFileMapEntry::parity_map(
                reference.tape_file_number,
                reference.block_count,
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
                .ok_or_else(|| {
                    filemark_scan_error("parity_map reference overlay object ordinals overflow")
                })?;
        } else {
            overlayed_entries.push(entry.clone());
        }
    }

    if !found_reference {
        return Err(filemark_scan_error(format!(
            "parity_map reference {} was not found in scanned map",
            reference.tape_file_number
        )));
    }

    FilemarkMap::new(overlayed_entries)
}

fn read_fixed_block_at_current_position(
    source: &mut dyn RawTapeSource,
    block_size: u32,
) -> Result<Option<Vec<u8>>, ParityError> {
    let block_size_usize = usize::try_from(block_size)
        .map_err(|_| ParityError::Invariant("scan block size does not fit usize"))?;
    let mut buf = vec![0u8; block_size_usize];
    match source.read_record(&mut buf) {
        Ok(RawReadOutcome::Block { bytes, .. }) if bytes == block_size_usize => Ok(Some(buf)),
        Ok(RawReadOutcome::Block { .. })
        | Ok(RawReadOutcome::Filemark { .. })
        | Ok(RawReadOutcome::EndOfData { .. })
        | Err(_) => Ok(None),
    }
}

fn apply_sidecar_directory_overlay(
    reconstructed: FilemarkMap,
    directory: &SidecarEpochDirectory,
    parity_map_reference: Option<&ParityMapReference>,
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
        if let Some(reference) = parity_map_reference {
            if reference.tape_file_number == entry.tape_file_number {
                if directory.entries.iter().any(|directory_entry| {
                    directory_entry.tape_file_number == entry.tape_file_number
                }) {
                    return Err(filemark_scan_error(format!(
                        "parity_map reference {} conflicts with sidecar directory entry",
                        reference.tape_file_number
                    )));
                }
                if entry.block_count != reference.block_count {
                    return Err(filemark_scan_error(format!(
                        "parity_map reference {} has block_count {}, scanned {}",
                        reference.tape_file_number, reference.block_count, entry.block_count
                    )));
                }
                overlayed_entries.push(TapeFileMapEntry::parity_map(
                    reference.tape_file_number,
                    reference.block_count,
                ));
                continue;
            }
        }

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
                TapeFileKind::Bootstrap | TapeFileKind::ParityMap
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

    let overlayed = FilemarkMap::new(overlayed_entries)?;
    let scope = overlayed.truncate_to_tape_files(directory.directory_scope_tape_file_count)?;
    if scope.total_data_ordinals() != directory.directory_scope_total_data_ordinals {
        return Err(filemark_scan_error(format!(
            "sidecar directory total ordinals {} do not match overlayed map {}",
            directory.directory_scope_total_data_ordinals,
            scope.total_data_ordinals()
        )));
    }
    if scope.max_sidecar_end_exclusive() != directory.directory_scope_highest_protected_ordinal {
        return Err(filemark_scan_error(format!(
            "sidecar directory protection watermark {} does not match overlayed map {}",
            directory.directory_scope_highest_protected_ordinal,
            scope.max_sidecar_end_exclusive()
        )));
    }

    Ok(overlayed)
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
    use crate::bootstrap::{
        write_bootstrap_block, BootstrapPayload, ParitySchemeRecord, BOOTSTRAP_HEADER_CRC_OFFSET,
        BOOTSTRAP_HEADER_LEN,
    };
    use crate::codec::ReedSolomonCodec;
    use crate::filemark_map::{
        FilemarkMapDigest, ScopedFilemarkMap, TapeFileKind, TapeFileMapEntry, TapeFilePosition,
    };
    use crate::model::{ParityScheme, SchemeId};
    use crate::parity_map::{
        encode_parity_map_tape_file, ParityMapPayload, ParityMapReference, SidecarEpochDirectory,
        SidecarEpochDirectoryEntry, PARITY_MAP_HEADER_CRC_OFFSET,
        SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD, SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
    };
    use crate::recovery::recover_object_block_from_sidecar;
    use crate::sidecar::{
        crc64_xz, data_shard_crc64, encode_sidecar_tape_file, EncodedSidecarTapeFile,
        SidecarDescriptor, SIDECAR_FOOTER_CRC_OFFSET, SIDECAR_HEADER_CRC_OFFSET,
    };
    use crate::source::{ObjectParitySource, OpenTrust};
    use remanence_library::BlockSource;

    const BLOCK_SIZE: u32 = 512;
    const TAPE_UUID: [u8; 16] = [0x42; 16];

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Record {
        Block(Vec<u8>),
        Filemark,
        Unreadable,
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
                Record::Unreadable => Err(remanence_library::TapeIoError::ReadBufferTooSmall {
                    actual: BLOCK_SIZE,
                    provided: BLOCK_SIZE / 2,
                }
                .into()),
            }
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.calls.push(ScanCall::Position(self.cursor as u64));
            Ok(PhysicalPositionHint::new(self.cursor as u64))
        }
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

    fn bootstrap_payload(digest: FilemarkMapDigest, sequence: u32) -> BootstrapPayload {
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
            sidecar_epoch_directory: None,
            parity_map_reference: None,
            object_rows: Vec::new(),
        }
    }

    fn bootstrap_block(digest: FilemarkMapDigest, sequence: u32) -> Vec<u8> {
        let payload = bootstrap_payload(digest, sequence);
        let mut block = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&payload, &mut block).expect("bootstrap block encodes");
        block
    }

    fn test_parity_map_reference(tape_file_number: u32, block_count: u64) -> ParityMapReference {
        ParityMapReference {
            tape_file_number,
            block_count,
            directory_scope_tape_file_count: tape_file_number.saturating_add(1),
            directory_scope_total_data_ordinals: 0,
            directory_scope_highest_protected_ordinal: 0,
            is_final_directory: true,
            parity_map_payload_sha256: [0xA5; 32],
            canonical_map_digest: [0x5A; 32],
        }
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
    fn parity_map_reference_overlay_errors_when_referenced_file_is_missing() {
        let reconstructed = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::bootstrap(2, 1),
        ])
        .expect("scan map validates");
        let reference = test_parity_map_reference(3, 2);

        let err = apply_parity_map_reference_structural_overlay(reconstructed, &reference)
            .expect_err("missing referenced parity_map must be an explicit scan error");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("parity_map reference 3 was not found in scanned map"),
                    "{message}"
                );
            }
            other => panic!("expected filemark-map reconstruction error, got {other:?}"),
        }
    }

    #[test]
    fn parity_map_reference_overlay_errors_on_block_count_mismatch() {
        let reconstructed = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::bootstrap(2, 1),
        ])
        .expect("scan map validates");
        let reference = test_parity_map_reference(1, 3);

        let err = apply_parity_map_reference_structural_overlay(reconstructed, &reference)
            .expect_err("reference block_count must match the scanned tape file");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("parity_map reference 1 has block_count 3, scanned 2"),
                    "{message}"
                );
            }
            other => panic!("expected filemark-map reconstruction error, got {other:?}"),
        }
    }

    #[test]
    fn parity_map_reference_overlay_refuses_structural_kind_conflicts() {
        let sidecar_conflict = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::parity_sidecar(2, 4, 0, 0, 2),
            TapeFileMapEntry::bootstrap(3, 1),
        ])
        .expect("scan map validates");
        let sidecar_reference = test_parity_map_reference(2, 4);

        let err =
            apply_parity_map_reference_structural_overlay(sidecar_conflict, &sidecar_reference)
                .expect_err("reference must not rewrite sidecars as parity_map");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("parity_map reference 2 conflicts with scanned ParitySidecar"),
                    "{message}"
                );
            }
            other => panic!("expected filemark-map reconstruction error, got {other:?}"),
        }

        let bootstrap_conflict = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::bootstrap(2, 1),
        ])
        .expect("scan map validates");
        let bootstrap_reference = test_parity_map_reference(2, 1);

        let err =
            apply_parity_map_reference_structural_overlay(bootstrap_conflict, &bootstrap_reference)
                .expect_err("reference must not rewrite bootstraps as parity_map");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("parity_map reference 2 conflicts with scanned Bootstrap"),
                    "{message}"
                );
            }
            other => panic!("expected filemark-map reconstruction error, got {other:?}"),
        }
    }

    fn sidecar_directory_entry(
        tape_file_number: u32,
        sidecar: &EncodedSidecarTapeFile,
    ) -> SidecarEpochDirectoryEntry {
        SidecarEpochDirectoryEntry {
            tape_file_number,
            epoch_id: sidecar.header.epoch_id,
            protected_ordinal_start: sidecar.header.protected_ordinal_start,
            protected_ordinal_end_exclusive: sidecar.header.protected_ordinal_end_exclusive,
            sidecar_total_block_count: sidecar.header.sidecar_total_block_count,
            sidecar_header_block_count: sidecar.header.shard_index_block_count,
            parity_shard_block_count: sidecar.header.parity_block_count,
            canonical_metadata_hash: sidecar.header.canonical_metadata_hash,
            flags: SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
                | SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
        }
    }

    fn sidecar_directory_for_map(
        map: &FilemarkMap,
        is_final_directory: bool,
        entries: Vec<SidecarEpochDirectoryEntry>,
    ) -> SidecarEpochDirectory {
        SidecarEpochDirectory {
            directory_scope_tape_file_count: map.tape_file_count(),
            directory_scope_total_data_ordinals: map.total_data_ordinals(),
            directory_scope_highest_protected_ordinal: map.max_sidecar_end_exclusive(),
            is_final_directory,
            entries,
        }
    }

    fn corrupt_sidecar_primary_and_footer(sidecar_blocks: &mut [Vec<u8>]) {
        sidecar_blocks[0][SIDECAR_HEADER_CRC_OFFSET] ^= 0xFF;
        let footer_index = sidecar_blocks
            .len()
            .checked_sub(1)
            .expect("sidecar must have a footer block");
        sidecar_blocks[footer_index][SIDECAR_FOOTER_CRC_OFFSET] ^= 0xFF;
    }

    fn corrupt_sidecar_primary_and_tail(
        sidecar: &EncodedSidecarTapeFile,
        sidecar_blocks: &mut [Vec<u8>],
    ) {
        sidecar_blocks[0][SIDECAR_HEADER_CRC_OFFSET] ^= 0xFF;
        let tail_index = usize::try_from(sidecar.header.tail_header_start_block)
            .expect("tail header start block fits usize");
        sidecar_blocks[tail_index][SIDECAR_HEADER_CRC_OFFSET] ^= 0xFF;
    }

    fn corrupt_sidecar_primary_tail_and_footer(
        sidecar: &EncodedSidecarTapeFile,
        sidecar_blocks: &mut [Vec<u8>],
    ) {
        corrupt_sidecar_primary_and_tail(sidecar, sidecar_blocks);
        let footer_index = sidecar_blocks
            .len()
            .checked_sub(1)
            .expect("sidecar must have a footer block");
        sidecar_blocks[footer_index][SIDECAR_FOOTER_CRC_OFFSET] ^= 0xFF;
    }

    fn corrupt_bootstrap_payload_crc(block: &mut [u8]) {
        assert!(
            has_bootstrap_magic(block),
            "test target must be a bootstrap"
        );
        let cbor_len = u32::from_le_bytes(block[0x28..0x2C].try_into().unwrap()) as usize;
        let payload_crc_offset = BOOTSTRAP_HEADER_LEN + cbor_len;
        assert!(
            payload_crc_offset < block.len(),
            "payload CRC offset must lie inside the bootstrap block"
        );
        block[payload_crc_offset] ^= 0xFF;
    }

    fn corrupt_bootstrap_cbor_payload_with_valid_crc(block: &mut [u8]) {
        assert!(
            has_bootstrap_magic(block),
            "test target must be a bootstrap"
        );
        let cbor_len = u32::from_le_bytes(block[0x28..0x2C].try_into().unwrap()) as usize;
        assert!(cbor_len > 0, "bootstrap CBOR payload must not be empty");
        let cbor_start = BOOTSTRAP_HEADER_LEN;
        let cbor_end = cbor_start
            .checked_add(cbor_len)
            .expect("CBOR payload end must not overflow");
        let payload_crc_len = std::mem::size_of::<u64>();
        let payload_crc_end = cbor_end
            .checked_add(payload_crc_len)
            .expect("payload CRC end must not overflow");
        assert!(
            cbor_end <= block.len(),
            "bootstrap CBOR payload must fit inside the block"
        );
        assert!(
            payload_crc_end <= block.len(),
            "payload CRC must fit after the bootstrap CBOR payload"
        );

        block[cbor_start] = 0xFF;
        let payload_crc = crc64_xz(&block[cbor_start..cbor_end]).to_le_bytes();
        block[cbor_end..payload_crc_end].copy_from_slice(&payload_crc);

        match parse_bootstrap_block(block) {
            Err(ParityError::BootstrapParse(message)) => {
                assert!(message.contains("CBOR"), "{message}");
            }
            other => panic!("corrupted CBOR payload should fail bootstrap parse, got {other:?}"),
        }
    }

    fn fixture_records(
        corrupt_bot_bootstrap_header: bool,
        corrupt_sidecar_header: bool,
    ) -> (Vec<Record>, FilemarkMap) {
        let object_a = vec![0xA0; BLOCK_SIZE as usize];
        let object_b = vec![0xB0; BLOCK_SIZE as usize];
        let descriptor = SidecarDescriptor {
            tape_uuid: TAPE_UUID,
            epoch_id: 0,
            k: 2,
            m: 1,
            stripes_per_epoch: 1,
            block_size: BLOCK_SIZE,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: 2,
        };
        let encoded_sidecar = encode_sidecar_tape_file(
            &descriptor,
            &[vec![0xC0; BLOCK_SIZE as usize]],
            vec![data_shard_crc64(&object_a), data_shard_crc64(&object_b)],
        )
        .expect("sidecar encodes");

        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::parity_sidecar(2, encoded_sidecar.blocks.len() as u64, 0, 0, 2),
            TapeFileMapEntry::bootstrap(3, 1),
        ])
        .expect("expected map validates");

        let prefix_map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let mut bot_bootstrap = bootstrap_block(prefix_map.digest(false).unwrap(), 0);
        if corrupt_bot_bootstrap_header {
            bot_bootstrap[BOOTSTRAP_HEADER_CRC_OFFSET] ^= 0xFF;
        }
        let final_bootstrap = bootstrap_block(expected_map.digest(true).unwrap(), 1);

        let mut sidecar_blocks = encoded_sidecar.blocks;
        if corrupt_sidecar_header {
            sidecar_blocks[0][SIDECAR_HEADER_CRC_OFFSET] ^= 0xFF;
        }

        let mut records = vec![
            Record::Block(bot_bootstrap),
            Record::Filemark,
            Record::Block(object_a),
            Record::Block(object_b),
            Record::Filemark,
        ];
        records.extend(sidecar_blocks.into_iter().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(final_bootstrap),
            Record::Filemark,
        ]);
        (records, expected_map)
    }

    fn multi_epoch_fixture_records(
        corrupt_sidecar_epoch: Option<u64>,
    ) -> (Vec<Record>, FilemarkMap) {
        let scheme = sample_scheme();
        let codec = ReedSolomonCodec::new(&scheme).expect("test scheme is valid");
        let epoch_data_shards =
            u64::from(scheme.data_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
        assert_eq!(
            epoch_data_shards, 2,
            "this fixture expects two object blocks per sidecar epoch"
        );

        let epoch_blocks = vec![
            vec![block(0x10), block(0x11)],
            vec![block(0x20), block(0x21)],
            vec![block(0x30), block(0x31)],
        ];
        let mut sidecars = Vec::new();
        for (epoch_index, object_blocks) in epoch_blocks.iter().enumerate() {
            let epoch_id = epoch_index as u64;
            let protected_ordinal_start = epoch_id * epoch_data_shards;
            let protected_ordinal_end_exclusive = protected_ordinal_start + epoch_data_shards;
            let descriptor = SidecarDescriptor {
                tape_uuid: TAPE_UUID,
                epoch_id,
                k: scheme.data_blocks_per_stripe,
                m: scheme.parity_blocks_per_stripe,
                stripes_per_epoch: scheme.stripes_per_neighborhood,
                block_size: BLOCK_SIZE,
                protected_ordinal_start,
                protected_ordinal_end_exclusive,
            };
            let parity_shards = codec.encode(object_blocks).expect("test parity encodes");
            let data_crcs = object_blocks
                .iter()
                .map(|block| data_shard_crc64(block))
                .collect();
            sidecars.push(
                encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs)
                    .expect("sidecar encodes"),
            );
        }

        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecars[0].blocks.len() as u64, 0, 0, 2),
            TapeFileMapEntry::object(3, 2, 2),
            TapeFileMapEntry::parity_sidecar(4, sidecars[1].blocks.len() as u64, 1, 2, 4),
            TapeFileMapEntry::object(5, 2, 4),
            TapeFileMapEntry::parity_sidecar(6, sidecars[2].blocks.len() as u64, 2, 4, 6),
            TapeFileMapEntry::bootstrap(7, 1),
        ])
        .expect("multi-epoch expected map validates");

        let prefix_map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let bot_bootstrap = bootstrap_block(prefix_map.digest(false).unwrap(), 0);
        let final_bootstrap = bootstrap_block(expected_map.digest(true).unwrap(), 1);

        let mut records = vec![Record::Block(bot_bootstrap), Record::Filemark];
        for (epoch_index, object_blocks) in epoch_blocks.into_iter().enumerate() {
            records.extend(object_blocks.into_iter().map(Record::Block));
            records.push(Record::Filemark);

            let mut sidecar_blocks = sidecars[epoch_index].blocks.clone();
            if corrupt_sidecar_epoch == Some(epoch_index as u64) {
                sidecar_blocks[0][SIDECAR_HEADER_CRC_OFFSET] ^= 0xFF;
            }
            records.extend(sidecar_blocks.into_iter().map(Record::Block));
            records.push(Record::Filemark);
        }
        records.extend([Record::Block(final_bootstrap), Record::Filemark]);

        (records, expected_map)
    }

    fn fixture_records_with_intermediate_bootstrap() -> (Vec<Record>, FilemarkMap, u32) {
        let scheme = sample_scheme();
        let codec = ReedSolomonCodec::new(&scheme).expect("test scheme is valid");
        let epoch_data_shards =
            u64::from(scheme.data_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
        assert_eq!(
            epoch_data_shards, 2,
            "this fixture expects two object blocks per sidecar epoch"
        );

        let epoch_blocks = [
            vec![block(0x40), block(0x41)],
            vec![block(0x50), block(0x51)],
        ];
        let mut sidecars = Vec::new();
        for (epoch_index, object_blocks) in epoch_blocks.iter().enumerate() {
            let epoch_id = epoch_index as u64;
            let protected_ordinal_start = epoch_id * epoch_data_shards;
            let protected_ordinal_end_exclusive = protected_ordinal_start + epoch_data_shards;
            let descriptor = SidecarDescriptor {
                tape_uuid: TAPE_UUID,
                epoch_id,
                k: scheme.data_blocks_per_stripe,
                m: scheme.parity_blocks_per_stripe,
                stripes_per_epoch: scheme.stripes_per_neighborhood,
                block_size: BLOCK_SIZE,
                protected_ordinal_start,
                protected_ordinal_end_exclusive,
            };
            let parity_shards = codec.encode(object_blocks).expect("test parity encodes");
            let data_crcs = object_blocks
                .iter()
                .map(|block| data_shard_crc64(block))
                .collect();
            sidecars.push(
                encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs)
                    .expect("sidecar encodes"),
            );
        }

        let intermediate_bootstrap_tape_file = 3;
        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecars[0].blocks.len() as u64, 0, 0, 2),
            TapeFileMapEntry::bootstrap(intermediate_bootstrap_tape_file, 1),
            TapeFileMapEntry::object(4, 2, 2),
            TapeFileMapEntry::parity_sidecar(5, sidecars[1].blocks.len() as u64, 1, 2, 4),
            TapeFileMapEntry::bootstrap(6, 1),
        ])
        .expect("three-bootstrap expected map validates");

        let bot_prefix = expected_map
            .truncate_to_tape_files(1)
            .expect("BOT prefix validates");
        let intermediate_prefix = expected_map
            .truncate_to_tape_files(intermediate_bootstrap_tape_file + 1)
            .expect("intermediate bootstrap prefix validates");
        let bot_bootstrap = bootstrap_block(bot_prefix.digest(false).unwrap(), 0);
        let intermediate_bootstrap = bootstrap_block(intermediate_prefix.digest(false).unwrap(), 1);
        let final_bootstrap = bootstrap_block(expected_map.digest(true).unwrap(), 2);

        let mut records = vec![Record::Block(bot_bootstrap), Record::Filemark];
        records.extend(epoch_blocks[0].iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(sidecars[0].blocks.iter().cloned().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(intermediate_bootstrap),
            Record::Filemark,
        ]);
        records.extend(epoch_blocks[1].iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(sidecars[1].blocks.iter().cloned().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(final_bootstrap),
            Record::Filemark,
        ]);

        (records, expected_map, intermediate_bootstrap_tape_file)
    }

    fn block(seed: u8) -> Vec<u8> {
        let mut block = vec![seed; BLOCK_SIZE as usize];
        block[0] = seed.wrapping_mul(17);
        block[1] = seed.wrapping_mul(31);
        block
    }

    fn recovery_sidecar_for_object(
        scheme: &ParityScheme,
        object_blocks: &[Vec<u8>],
    ) -> crate::sidecar::EncodedSidecarTapeFile {
        let codec = ReedSolomonCodec::new(scheme).expect("test scheme is valid");
        let mut parity_shards = Vec::new();
        for stripe in 0..scheme.stripes_per_neighborhood as usize {
            let mut data = Vec::new();
            for row in 0..scheme.data_blocks_per_stripe as usize {
                let ordinal = row * scheme.stripes_per_neighborhood as usize + stripe;
                data.push(object_blocks[ordinal].clone());
            }
            parity_shards.extend(codec.encode(&data).expect("test parity encodes"));
        }

        let descriptor = SidecarDescriptor {
            tape_uuid: TAPE_UUID,
            epoch_id: 0,
            k: scheme.data_blocks_per_stripe,
            m: scheme.parity_blocks_per_stripe,
            stripes_per_epoch: scheme.stripes_per_neighborhood,
            block_size: BLOCK_SIZE,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: object_blocks.len() as u64,
        };
        let data_crcs = object_blocks
            .iter()
            .map(|block| data_shard_crc64(block))
            .collect();
        encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs)
            .expect("test sidecar encodes")
    }

    fn catalog_recovery_fixture_records() -> (Vec<Record>, FilemarkMap, Vec<Vec<u8>>) {
        let scheme = sample_scheme();
        let object_blocks = vec![block(1), block(2)];
        let sidecar = recovery_sidecar_for_object(&scheme, &object_blocks);
        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, object_blocks.len() as u64, 0),
            TapeFileMapEntry::parity_sidecar(
                2,
                sidecar.blocks.len() as u64,
                0,
                0,
                object_blocks.len() as u64,
            ),
            TapeFileMapEntry::bootstrap(3, 1),
        ])
        .expect("catalog recovery map validates");

        let prefix_map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let bot_bootstrap = bootstrap_block(prefix_map.digest(false).unwrap(), 0);
        let mut final_bootstrap = bootstrap_block(expected_map.digest(true).unwrap(), 1);
        final_bootstrap[BOOTSTRAP_HEADER_CRC_OFFSET] ^= 0xFF;

        let mut records = vec![Record::Block(bot_bootstrap), Record::Filemark];
        records.extend(object_blocks.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(sidecar.blocks.into_iter().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(final_bootstrap),
            Record::Filemark,
        ]);

        (records, expected_map, object_blocks)
    }

    #[test]
    fn scan_reconstructs_map_and_validates_against_final_bootstrap_digest() {
        let (records, expected_map) = fixture_records(false, false);
        let final_digest = expected_map.digest(true).unwrap();
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_eq!(reconstructed, expected_map);
        let scoped =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);
        assert_eq!(
            source
                .calls
                .iter()
                .filter(|call| matches!(call, ScanCall::SpaceFilemarks(1)))
                .count(),
            4
        );
    }

    #[test]
    fn scan_treats_unreadable_object_head_as_object_candidate() {
        let (mut records, expected_map) = fixture_records(false, false);
        let final_digest = expected_map.digest(true).unwrap();
        records[2] = Record::Unreadable;
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_eq!(reconstructed, expected_map);
        ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
    }

    #[test]
    fn scan_classifies_valid_parity_map_control_file() {
        let directory = SidecarEpochDirectory {
            directory_scope_tape_file_count: 3,
            directory_scope_total_data_ordinals: 0,
            directory_scope_highest_protected_ordinal: 0,
            is_final_directory: true,
            entries: Vec::new(),
        };
        let parity_map_payload = ParityMapPayload {
            tape_uuid: TAPE_UUID,
            sequence: 1,
            directory,
            canonical_map_digest: [0xAB; 32],
            writer_version: Some("scan-test".to_string()),
            write_timestamp: None,
        };
        let encoded_parity_map =
            encode_parity_map_tape_file(&parity_map_payload, BLOCK_SIZE).unwrap();
        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::parity_map(1, encoded_parity_map.blocks.len() as u64),
            TapeFileMapEntry::bootstrap(2, 1),
        ])
        .expect("map with parity_map validates");
        let bot_prefix =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let mut records = vec![
            Record::Block(bootstrap_block(bot_prefix.digest(false).unwrap(), 0)),
            Record::Filemark,
        ];
        records.extend(encoded_parity_map.blocks.into_iter().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(bootstrap_block(expected_map.digest(true).unwrap(), 2)),
            Record::Filemark,
        ]);
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_eq!(reconstructed, expected_map);
        assert_eq!(reconstructed.entries()[1].kind, TapeFileKind::ParityMap);
    }

    #[test]
    fn acquire_filemark_map_uses_inline_directory_overlay_for_fully_damaged_sidecar_metadata() {
        let object_a = block(0x71);
        let object_b = block(0x72);
        let descriptor = SidecarDescriptor {
            tape_uuid: TAPE_UUID,
            epoch_id: 0,
            k: 2,
            m: 1,
            stripes_per_epoch: 1,
            block_size: BLOCK_SIZE,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: 2,
        };
        let encoded_sidecar = encode_sidecar_tape_file(
            &descriptor,
            &[vec![0xC7; BLOCK_SIZE as usize]],
            vec![data_shard_crc64(&object_a), data_shard_crc64(&object_b)],
        )
        .expect("sidecar encodes");
        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::parity_sidecar(2, encoded_sidecar.blocks.len() as u64, 0, 0, 2),
            TapeFileMapEntry::bootstrap(3, 1),
        ])
        .expect("expected map validates");
        let directory = sidecar_directory_for_map(
            &expected_map,
            true,
            vec![sidecar_directory_entry(2, &encoded_sidecar)],
        );
        let final_digest = expected_map.digest(true).expect("final digest builds");
        let mut final_payload = bootstrap_payload(final_digest.clone(), 1);
        final_payload.sidecar_epoch_directory = Some(directory);

        let prefix_map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let mut sidecar_blocks = encoded_sidecar.blocks.clone();
        corrupt_sidecar_primary_and_footer(&mut sidecar_blocks);

        let mut records = vec![
            Record::Block(bootstrap_block(prefix_map.digest(false).unwrap(), 0)),
            Record::Filemark,
            Record::Block(object_a),
            Record::Block(object_b),
            Record::Filemark,
        ];
        records.extend(sidecar_blocks.into_iter().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(bootstrap_block_for_payload(&final_payload)),
            Record::Filemark,
        ]);

        let mut scan_source = RecordingRawSource::new(records.clone());
        let reconstructed = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("scan without directory overlay still completes");
        assert_ne!(reconstructed, expected_map);
        assert_eq!(reconstructed.entries()[2].kind, TapeFileKind::Object);
        assert!(matches!(
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest),
            Err(ParityError::FilemarkMapDigestMismatch)
        ));

        let mut source = RecordingRawSource::new(records);
        let scoped = acquire_filemark_map(&mut source, &final_payload, None)
            .expect("inline directory repairs sidecar classification before digest validation");

        assert_eq!(scoped.map, expected_map);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);
    }

    #[test]
    fn catalog_less_recovery_isolates_fully_damaged_sidecar_metadata_to_one_epoch() {
        let scheme = sample_scheme();
        let codec = ReedSolomonCodec::new(&scheme).expect("test scheme is valid");
        let epoch_data_shards =
            u64::from(scheme.data_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
        let epoch0_blocks = vec![block(0x91), block(0x92)];
        let epoch1_blocks = vec![block(0xA1), block(0xA2)];
        let encode_epoch =
            |epoch_id: u64, blocks: &[Vec<u8>]| -> crate::sidecar::EncodedSidecarTapeFile {
                let start = epoch_id
                    .checked_mul(epoch_data_shards)
                    .expect("epoch start fits u64");
                let descriptor = SidecarDescriptor {
                    tape_uuid: TAPE_UUID,
                    epoch_id,
                    k: scheme.data_blocks_per_stripe,
                    m: scheme.parity_blocks_per_stripe,
                    stripes_per_epoch: scheme.stripes_per_neighborhood,
                    block_size: BLOCK_SIZE,
                    protected_ordinal_start: start,
                    protected_ordinal_end_exclusive: start + blocks.len() as u64,
                };
                let parity_shards = codec.encode(blocks).expect("test parity encodes");
                let data_crcs = blocks.iter().map(|block| data_shard_crc64(block)).collect();
                encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs)
                    .expect("test sidecar encodes")
            };
        let sidecar0 = encode_epoch(0, &epoch0_blocks);
        let sidecar1 = encode_epoch(1, &epoch1_blocks);
        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, epoch0_blocks.len() as u64, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar0.blocks.len() as u64, 0, 0, 2),
            TapeFileMapEntry::object(3, epoch1_blocks.len() as u64, 2),
            TapeFileMapEntry::parity_sidecar(4, sidecar1.blocks.len() as u64, 1, 2, 4),
            TapeFileMapEntry::bootstrap(5, 1),
        ])
        .expect("multi-epoch map validates");
        let directory = sidecar_directory_for_map(
            &expected_map,
            true,
            vec![
                sidecar_directory_entry(2, &sidecar0),
                sidecar_directory_entry(4, &sidecar1),
            ],
        );
        let final_digest = expected_map.digest(true).expect("final digest builds");
        let mut final_payload = bootstrap_payload(final_digest.clone(), 1);
        final_payload.sidecar_epoch_directory = Some(directory);

        let prefix_map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let mut damaged_sidecar0_blocks = sidecar0.blocks.clone();
        corrupt_sidecar_primary_tail_and_footer(&sidecar0, &mut damaged_sidecar0_blocks);

        let mut records = vec![
            Record::Block(bootstrap_block(prefix_map.digest(false).unwrap(), 0)),
            Record::Filemark,
        ];
        records.extend(epoch0_blocks.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(damaged_sidecar0_blocks.into_iter().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(epoch1_blocks.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(sidecar1.blocks.iter().cloned().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(bootstrap_block_for_payload(&final_payload)),
            Record::Filemark,
        ]);

        let mut source = RecordingRawSource::new(records.clone());
        let scoped = acquire_filemark_map(&mut source, &final_payload, None)
            .expect("inline directory repairs damaged sidecar classification");
        assert_eq!(scoped.map, expected_map);
        assert_eq!(scoped.scope.watermark(), 4);

        let mut bad_epoch_source = RecordingRawSource::new(records.clone());
        let mut bad_epoch = ObjectParitySource::open(
            &mut bad_epoch_source,
            scheme.clone(),
            TAPE_UUID,
            scoped.clone(),
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("bad epoch object source opens");
        let err = bad_epoch
            .recover_block_at(0)
            .expect_err("fully damaged sidecar metadata makes only epoch 0 unavailable");
        assert!(matches!(
            err,
            ParityError::SidecarMetadataUnavailable { epoch_id: 0 }
        ));

        let mut good_epoch_source = RecordingRawSource::new(records);
        let mut good_epoch = ObjectParitySource::open(
            &mut good_epoch_source,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            3,
            OpenTrust::RequireValidated,
        )
        .expect("good epoch object source opens");
        let recovered = good_epoch
            .recover_block_at(0)
            .expect("unrelated epoch still recovers");
        assert_eq!(recovered, epoch1_blocks[0]);
    }

    #[test]
    fn catalog_less_recovery_isolates_footer_only_sidecar_metadata_loss_to_one_epoch() {
        let scheme = sample_scheme();
        let codec = ReedSolomonCodec::new(&scheme).expect("test scheme is valid");
        let epoch_data_shards =
            u64::from(scheme.data_blocks_per_stripe) * u64::from(scheme.stripes_per_neighborhood);
        let epoch0_blocks = vec![block(0xB1), block(0xB2)];
        let epoch1_blocks = vec![block(0xC1), block(0xC2)];
        let encode_epoch =
            |epoch_id: u64, blocks: &[Vec<u8>]| -> crate::sidecar::EncodedSidecarTapeFile {
                let start = epoch_id
                    .checked_mul(epoch_data_shards)
                    .expect("epoch start fits u64");
                let descriptor = SidecarDescriptor {
                    tape_uuid: TAPE_UUID,
                    epoch_id,
                    k: scheme.data_blocks_per_stripe,
                    m: scheme.parity_blocks_per_stripe,
                    stripes_per_epoch: scheme.stripes_per_neighborhood,
                    block_size: BLOCK_SIZE,
                    protected_ordinal_start: start,
                    protected_ordinal_end_exclusive: start + blocks.len() as u64,
                };
                let parity_shards = codec.encode(blocks).expect("test parity encodes");
                let data_crcs = blocks.iter().map(|block| data_shard_crc64(block)).collect();
                encode_sidecar_tape_file(&descriptor, &parity_shards, data_crcs)
                    .expect("test sidecar encodes")
            };
        let sidecar0 = encode_epoch(0, &epoch0_blocks);
        let sidecar1 = encode_epoch(1, &epoch1_blocks);
        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, epoch0_blocks.len() as u64, 0),
            TapeFileMapEntry::parity_sidecar(2, sidecar0.blocks.len() as u64, 0, 0, 2),
            TapeFileMapEntry::object(3, epoch1_blocks.len() as u64, 2),
            TapeFileMapEntry::parity_sidecar(4, sidecar1.blocks.len() as u64, 1, 2, 4),
            TapeFileMapEntry::bootstrap(5, 1),
        ])
        .expect("multi-epoch map validates");
        let directory = sidecar_directory_for_map(
            &expected_map,
            true,
            vec![
                sidecar_directory_entry(2, &sidecar0),
                sidecar_directory_entry(4, &sidecar1),
            ],
        );
        let final_digest = expected_map.digest(true).expect("final digest builds");
        let mut final_payload = bootstrap_payload(final_digest.clone(), 1);
        final_payload.sidecar_epoch_directory = Some(directory);

        let prefix_map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let mut damaged_sidecar0_blocks = sidecar0.blocks.clone();
        corrupt_sidecar_primary_and_tail(&sidecar0, &mut damaged_sidecar0_blocks);

        let mut records = vec![
            Record::Block(bootstrap_block(prefix_map.digest(false).unwrap(), 0)),
            Record::Filemark,
        ];
        records.extend(epoch0_blocks.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(damaged_sidecar0_blocks.into_iter().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(epoch1_blocks.iter().cloned().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(sidecar1.blocks.iter().cloned().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(bootstrap_block_for_payload(&final_payload)),
            Record::Filemark,
        ]);

        let mut scan_source = RecordingRawSource::new(records.clone());
        let reconstructed = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("footer locator classifies the sidecar structurally");
        assert_eq!(reconstructed, expected_map);
        assert_eq!(reconstructed.entries()[2].kind, TapeFileKind::ParitySidecar);

        let mut source = RecordingRawSource::new(records.clone());
        let scoped = acquire_filemark_map(&mut source, &final_payload, None)
            .expect("footer-classified map validates before recovery opens sidecars");
        assert_eq!(scoped.map, expected_map);
        assert_eq!(scoped.scope.watermark(), 4);

        let mut bad_epoch_source = RecordingRawSource::new(records.clone());
        let mut bad_epoch = ObjectParitySource::open(
            &mut bad_epoch_source,
            scheme.clone(),
            TAPE_UUID,
            scoped.clone(),
            BLOCK_SIZE,
            1,
            OpenTrust::RequireValidated,
        )
        .expect("bad epoch object source opens");
        let err = bad_epoch
            .recover_block_at(0)
            .expect_err("footer-only damaged sidecar metadata makes only epoch 0 unavailable");
        assert!(matches!(
            err,
            ParityError::SidecarMetadataUnavailable { epoch_id: 0 }
        ));

        let mut good_epoch_source = RecordingRawSource::new(records);
        let mut good_epoch = ObjectParitySource::open(
            &mut good_epoch_source,
            scheme,
            TAPE_UUID,
            scoped,
            BLOCK_SIZE,
            3,
            OpenTrust::RequireValidated,
        )
        .expect("good epoch object source opens");
        let recovered = good_epoch
            .recover_block_at(0)
            .expect("unrelated epoch still recovers");
        assert_eq!(recovered, epoch1_blocks[0]);
    }

    #[test]
    fn acquire_filemark_map_uses_referenced_parity_map_overlay_when_control_primary_is_damaged() {
        let object_a = block(0x81);
        let object_b = block(0x82);
        let descriptor = SidecarDescriptor {
            tape_uuid: TAPE_UUID,
            epoch_id: 0,
            k: 2,
            m: 1,
            stripes_per_epoch: 1,
            block_size: BLOCK_SIZE,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: 2,
        };
        let encoded_sidecar = encode_sidecar_tape_file(
            &descriptor,
            &[vec![0xD7; BLOCK_SIZE as usize]],
            vec![data_shard_crc64(&object_a), data_shard_crc64(&object_b)],
        )
        .expect("sidecar encodes");
        let provisional_directory = SidecarEpochDirectory {
            directory_scope_tape_file_count: 5,
            directory_scope_total_data_ordinals: 2,
            directory_scope_highest_protected_ordinal: 2,
            is_final_directory: true,
            entries: vec![sidecar_directory_entry(2, &encoded_sidecar)],
        };
        let provisional_parity_map = encode_parity_map_tape_file(
            &ParityMapPayload {
                tape_uuid: TAPE_UUID,
                sequence: 1,
                directory: provisional_directory.clone(),
                canonical_map_digest: [0; 32],
                writer_version: Some("scan-test".to_string()),
                write_timestamp: None,
            },
            BLOCK_SIZE,
        )
        .expect("provisional parity_map encodes");
        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::parity_sidecar(2, encoded_sidecar.blocks.len() as u64, 0, 0, 2),
            TapeFileMapEntry::parity_map(3, provisional_parity_map.blocks.len() as u64),
            TapeFileMapEntry::bootstrap(4, 1),
        ])
        .expect("expected map with parity_map validates");
        let directory = sidecar_directory_for_map(
            &expected_map,
            true,
            vec![sidecar_directory_entry(2, &encoded_sidecar)],
        );
        let final_digest = expected_map.digest(true).expect("final digest builds");
        let mut encoded_parity_map = encode_parity_map_tape_file(
            &ParityMapPayload {
                tape_uuid: TAPE_UUID,
                sequence: 1,
                directory: directory.clone(),
                canonical_map_digest: final_digest.map_sha256,
                writer_version: Some("scan-test".to_string()),
                write_timestamp: None,
            },
            BLOCK_SIZE,
        )
        .expect("final parity_map encodes");
        assert_eq!(
            encoded_parity_map.blocks.len(),
            provisional_parity_map.blocks.len(),
            "fixed-width digest replacement must not change parity_map block count"
        );
        encoded_parity_map.blocks[0][PARITY_MAP_HEADER_CRC_OFFSET] ^= 0xFF;

        let mut final_payload = bootstrap_payload(final_digest.clone(), 2);
        final_payload.parity_map_reference = Some(ParityMapReference {
            tape_file_number: 3,
            block_count: encoded_parity_map.blocks.len() as u64,
            directory_scope_tape_file_count: directory.directory_scope_tape_file_count,
            directory_scope_total_data_ordinals: directory.directory_scope_total_data_ordinals,
            directory_scope_highest_protected_ordinal: directory
                .directory_scope_highest_protected_ordinal,
            is_final_directory: directory.is_final_directory,
            parity_map_payload_sha256: encoded_parity_map.header.payload_sha256,
            canonical_map_digest: final_digest.map_sha256,
        });

        let prefix_map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let mut sidecar_blocks = encoded_sidecar.blocks.clone();
        corrupt_sidecar_primary_and_footer(&mut sidecar_blocks);

        let mut records = vec![
            Record::Block(bootstrap_block(prefix_map.digest(false).unwrap(), 0)),
            Record::Filemark,
            Record::Block(object_a),
            Record::Block(object_b),
            Record::Filemark,
        ];
        records.extend(sidecar_blocks.into_iter().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(encoded_parity_map.blocks.into_iter().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(bootstrap_block_for_payload(&final_payload)),
            Record::Filemark,
        ]);

        let mut scan_source = RecordingRawSource::new(records.clone());
        let reconstructed = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("scan without directory overlay still completes");
        assert_ne!(reconstructed, expected_map);
        assert_eq!(reconstructed.entries()[2].kind, TapeFileKind::Object);
        assert_eq!(reconstructed.entries()[3].kind, TapeFileKind::Object);

        let mut source = RecordingRawSource::new(records);
        let scoped = acquire_filemark_map(&mut source, &final_payload, None)
            .expect("parity_map reference repairs sidecar and control-file classifications");

        assert_eq!(scoped.map, expected_map);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);
    }

    #[test]
    fn acquire_filemark_map_falls_back_to_sidecar_tail_when_referenced_parity_map_is_unreadable() {
        let scheme = sample_scheme();
        let codec = ReedSolomonCodec::new(&scheme).expect("test scheme is valid");
        let object_a = block(0x91);
        let object_b = block(0x92);
        let parity_shards = codec
            .encode(&[object_a.clone(), object_b.clone()])
            .expect("test parity encodes");
        let descriptor = SidecarDescriptor {
            tape_uuid: TAPE_UUID,
            epoch_id: 0,
            k: scheme.data_blocks_per_stripe,
            m: scheme.parity_blocks_per_stripe,
            stripes_per_epoch: scheme.stripes_per_neighborhood,
            block_size: BLOCK_SIZE,
            protected_ordinal_start: 0,
            protected_ordinal_end_exclusive: 2,
        };
        let encoded_sidecar = encode_sidecar_tape_file(
            &descriptor,
            &parity_shards,
            vec![data_shard_crc64(&object_a), data_shard_crc64(&object_b)],
        )
        .expect("sidecar encodes");
        let provisional_directory = SidecarEpochDirectory {
            directory_scope_tape_file_count: 5,
            directory_scope_total_data_ordinals: 2,
            directory_scope_highest_protected_ordinal: 2,
            is_final_directory: true,
            entries: vec![sidecar_directory_entry(2, &encoded_sidecar)],
        };
        let provisional_parity_map = encode_parity_map_tape_file(
            &ParityMapPayload {
                tape_uuid: TAPE_UUID,
                sequence: 1,
                directory: provisional_directory,
                canonical_map_digest: [0; 32],
                writer_version: Some("scan-test".to_string()),
                write_timestamp: None,
            },
            BLOCK_SIZE,
        )
        .expect("provisional parity_map encodes");
        let expected_map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 2, 0),
            TapeFileMapEntry::parity_sidecar(2, encoded_sidecar.blocks.len() as u64, 0, 0, 2),
            TapeFileMapEntry::parity_map(3, provisional_parity_map.blocks.len() as u64),
            TapeFileMapEntry::bootstrap(4, 1),
        ])
        .expect("expected map with parity_map validates");
        let directory = sidecar_directory_for_map(
            &expected_map,
            true,
            vec![sidecar_directory_entry(2, &encoded_sidecar)],
        );
        let final_digest = expected_map.digest(true).expect("final digest builds");
        let mut encoded_parity_map = encode_parity_map_tape_file(
            &ParityMapPayload {
                tape_uuid: TAPE_UUID,
                sequence: 1,
                directory: directory.clone(),
                canonical_map_digest: final_digest.map_sha256,
                writer_version: Some("scan-test".to_string()),
                write_timestamp: None,
            },
            BLOCK_SIZE,
        )
        .expect("final parity_map encodes");
        assert_eq!(
            encoded_parity_map.blocks.len(),
            provisional_parity_map.blocks.len(),
            "fixed-width digest replacement must not change parity_map block count"
        );
        let parity_map_tail_start =
            usize::try_from(encoded_parity_map.header.tail_copy_start_block)
                .expect("tail copy start fits usize");
        encoded_parity_map.blocks[0][PARITY_MAP_HEADER_CRC_OFFSET] ^= 0xFF;
        encoded_parity_map.blocks[parity_map_tail_start][PARITY_MAP_HEADER_CRC_OFFSET] ^= 0xFF;

        let mut final_payload = bootstrap_payload(final_digest.clone(), 2);
        final_payload.parity_map_reference = Some(ParityMapReference {
            tape_file_number: 3,
            block_count: encoded_parity_map.blocks.len() as u64,
            directory_scope_tape_file_count: directory.directory_scope_tape_file_count,
            directory_scope_total_data_ordinals: directory.directory_scope_total_data_ordinals,
            directory_scope_highest_protected_ordinal: directory
                .directory_scope_highest_protected_ordinal,
            is_final_directory: directory.is_final_directory,
            parity_map_payload_sha256: encoded_parity_map.header.payload_sha256,
            canonical_map_digest: final_digest.map_sha256,
        });

        let prefix_map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let mut sidecar_blocks = encoded_sidecar.blocks.clone();
        sidecar_blocks[0][SIDECAR_HEADER_CRC_OFFSET] ^= 0xFF;

        let mut records = vec![
            Record::Block(bootstrap_block(prefix_map.digest(false).unwrap(), 0)),
            Record::Filemark,
            Record::Block(object_a.clone()),
            Record::Block(object_b.clone()),
            Record::Filemark,
        ];
        records.extend(sidecar_blocks.into_iter().map(Record::Block));
        records.push(Record::Filemark);
        records.extend(encoded_parity_map.blocks.into_iter().map(Record::Block));
        records.extend([
            Record::Filemark,
            Record::Block(bootstrap_block_for_payload(&final_payload)),
            Record::Filemark,
        ]);

        let mut scan_source = RecordingRawSource::new(records.clone());
        let reconstructed = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("scan still classifies the sidecar from tail metadata");
        assert_ne!(reconstructed, expected_map);
        assert_eq!(reconstructed.entries()[2].kind, TapeFileKind::ParitySidecar);
        assert_eq!(reconstructed.entries()[3].kind, TapeFileKind::Object);
        assert!(matches!(
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest),
            Err(ParityError::FilemarkMapDigestMismatch)
        ));

        let mut source = RecordingRawSource::new(records);
        let scoped = acquire_filemark_map(&mut source, &final_payload, None)
            .expect("parity_map reference preserves control-file structure while sidecar tail metadata supplies recovery rows");
        assert_eq!(scoped.map, expected_map);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);

        let recovered = recover_object_block_from_sidecar(
            &mut source,
            &scoped,
            &scheme,
            TAPE_UUID,
            BLOCK_SIZE,
            1,
            1,
        )
        .expect("unreadable parity_map does not block sidecar-tail recovery");
        assert_eq!(recovered.recovered_block, object_b);
        assert_eq!(recovered.sidecar_tape_file_number, 2);
    }

    #[test]
    fn acquire_filemark_map_uses_catalog_without_scan() {
        let (_records, expected_map) = fixture_records(false, false);
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);
        let watermark = expected_map.max_sidecar_end_exclusive();
        let mut source = RecordingRawSource::new(Vec::new());

        let scoped = acquire_filemark_map(
            &mut source,
            &final_payload,
            Some(CatalogFilemarkMapInput::new(
                TAPE_UUID,
                expected_map.clone(),
                watermark,
            )),
        )
        .expect("catalog map is accepted");

        assert_eq!(scoped.map, expected_map);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), watermark);
        assert!(source.calls.is_empty());
    }

    #[test]
    fn catalog_present_recovery_succeeds_when_scan_path_is_damaged() {
        let (records, expected_map, object_blocks) = catalog_recovery_fixture_records();
        let final_digest = expected_map.digest(true).unwrap();
        let final_payload = bootstrap_payload(final_digest.clone(), 1);

        let mut scan_source = RecordingRawSource::new(records.clone());
        let reconstructed = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("damaged final bootstrap still yields a structural candidate map");
        assert_ne!(reconstructed, expected_map);
        assert!(matches!(
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest),
            Err(ParityError::FilemarkMapDigestMismatch)
        ));

        let mut source = RecordingRawSource::new(records);
        let scoped = acquire_filemark_map(
            &mut source,
            &final_payload,
            Some(CatalogFilemarkMapInput::new(
                TAPE_UUID,
                expected_map.clone(),
                expected_map.max_sidecar_end_exclusive(),
            )),
        )
        .expect("catalog map bypasses the damaged scan path");
        assert!(source.calls.is_empty(), "catalog acquisition must not scan");

        let recovered = recover_object_block_from_sidecar(
            &mut source,
            &scoped,
            &sample_scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            1,
            1,
        )
        .expect("catalog-present sidecar recovery succeeds");

        assert_eq!(recovered.recovered_block, object_blocks[1]);
        assert_eq!(recovered.failed_ordinal, 1);
        assert_eq!(recovered.sidecar_tape_file_number, 2);
    }

    #[test]
    fn acquire_filemark_map_rejects_catalog_tape_uuid_mismatch() {
        let (_records, expected_map) = fixture_records(false, false);
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);
        let watermark = expected_map.max_sidecar_end_exclusive();
        let mut wrong_tape_uuid = TAPE_UUID;
        wrong_tape_uuid[0] ^= 0x80;
        let mut source = RecordingRawSource::new(Vec::new());

        let err = acquire_filemark_map(
            &mut source,
            &final_payload,
            Some(CatalogFilemarkMapInput::new(
                wrong_tape_uuid,
                expected_map,
                watermark,
            )),
        )
        .expect_err("catalog tape UUID must match bootstrap");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("catalog tape UUID does not match"),
                    "{message}"
                );
            }
            other => panic!("expected filemark map error, got {other:?}"),
        }
        assert!(source.calls.is_empty());
    }

    #[test]
    fn acquire_filemark_map_rejects_incoherent_catalog_watermark() {
        let (_records, expected_map) = fixture_records(false, false);
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);
        let mut source = RecordingRawSource::new(Vec::new());

        let err = acquire_filemark_map(
            &mut source,
            &final_payload,
            Some(CatalogFilemarkMapInput::new(TAPE_UUID, expected_map, 1)),
        )
        .expect_err("catalog watermark must match sidecar watermark");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("does not match sidecar watermark"),
                    "{message}"
                );
            }
            other => panic!("expected filemark map error, got {other:?}"),
        }
        assert!(source.calls.is_empty());
    }

    #[test]
    fn acquire_filemark_map_scans_and_validates_final_bootstrap_digest() {
        let (records, expected_map) = fixture_records(false, false);
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);
        let mut source = RecordingRawSource::new(records);

        let scoped =
            acquire_filemark_map(&mut source, &final_payload, None).expect("scan validates");

        assert_eq!(scoped.map, expected_map);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);
    }

    #[test]
    fn acquire_filemark_map_preserves_intermediate_prefix_scope() {
        let (records, expected_map) = fixture_records(false, false);
        let prefix = expected_map
            .truncate_to_tape_files(2)
            .expect("prefix validates");
        let prefix_payload = bootstrap_payload(prefix.digest(false).unwrap(), 1);
        let mut source = RecordingRawSource::new(records);

        let scoped =
            acquire_filemark_map(&mut source, &prefix_payload, None).expect("prefix validates");

        assert_eq!(scoped.map, expected_map);
        assert_eq!(scoped.validated_prefix_tape_files, Some(2));
        assert!(scoped.is_validated(1));
        assert!(!scoped.is_validated(2));
        let err = scoped.scope.recoverable(0).unwrap_err();
        assert!(matches!(
            err,
            ParityError::UnrecoverablePendingEpoch {
                failed_ordinal: 0,
                watermark: 0
            }
        ));
    }

    #[test]
    fn bootstrap_scope_controls_validated_and_tar_only_object_access() {
        let (records, expected_map) = multi_epoch_fixture_records(None);
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);
        let mut final_source = RecordingRawSource::new(records.clone());

        let full_scope = acquire_filemark_map(&mut final_source, &final_payload, None)
            .expect("final bootstrap validates full reconstructed map");
        assert_eq!(full_scope.validated_prefix_tape_files, None);
        assert!(full_scope.is_validated(5));

        let mut full_object = ObjectParitySource::open(
            &mut final_source,
            sample_scheme(),
            TAPE_UUID,
            full_scope,
            BLOCK_SIZE,
            5,
            OpenTrust::RequireValidated,
        )
        .expect("final scope permits validated access to the last object");
        assert!(!full_object.is_tar_only_unverified());
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        full_object
            .read_block(&mut buf)
            .expect("validated full-scope object read succeeds");
        assert_eq!(buf, block(0x30));

        let prefix = expected_map
            .truncate_to_tape_files(3)
            .expect("prefix through first sidecar validates");
        let prefix_payload = bootstrap_payload(prefix.digest(false).unwrap(), 2);
        let mut prefix_source = RecordingRawSource::new(records);
        let prefix_scope = acquire_filemark_map(&mut prefix_source, &prefix_payload, None)
            .expect("intermediate bootstrap validates only its prefix");

        assert_eq!(prefix_scope.validated_prefix_tape_files, Some(3));
        assert!(prefix_scope.is_validated(2));
        assert!(!prefix_scope.is_validated(3));

        let rejected = match ObjectParitySource::open(
            &mut prefix_source,
            sample_scheme(),
            TAPE_UUID,
            prefix_scope.clone(),
            BLOCK_SIZE,
            3,
            OpenTrust::RequireValidated,
        ) {
            Err(err) => err,
            Ok(_) => panic!("require-validated rejects objects outside the prefix"),
        };
        assert!(matches!(
            rejected,
            ParityError::OutsideValidatedMapPrefix {
                ordinal: 2,
                prefix_ordinals: 2
            }
        ));

        let mut tar_only = ObjectParitySource::open(
            &mut prefix_source,
            sample_scheme(),
            TAPE_UUID,
            prefix_scope,
            BLOCK_SIZE,
            3,
            OpenTrust::AllowTarOnlyUnverified,
        )
        .expect("tar-only unverified access is allowed outside the prefix");
        assert!(tar_only.is_tar_only_unverified());
        tar_only
            .read_block(&mut buf)
            .expect("clean tar-only suffix read succeeds");
        assert_eq!(buf, block(0x20));

        let err = tar_only
            .recover_block_at(0)
            .expect_err("parity recovery remains disabled outside the prefix");
        assert!(matches!(
            err,
            ParityError::OutsideValidatedMapPrefix {
                ordinal: 2,
                prefix_ordinals: 2
            }
        ));
    }

    #[test]
    fn deeper_bootstrap_prefix_validates_second_epoch_and_fences_suffix() {
        let (records, expected_map) = multi_epoch_fixture_records(None);
        let prefix = expected_map
            .truncate_to_tape_files(5)
            .expect("prefix through second sidecar validates");
        let prefix_payload = bootstrap_payload(prefix.digest(false).unwrap(), 2);
        let mut source = RecordingRawSource::new(records);

        let prefix_scope = acquire_filemark_map(&mut source, &prefix_payload, None)
            .expect("deeper intermediate bootstrap validates its prefix");

        assert_eq!(prefix_scope.validated_prefix_tape_files, Some(5));
        assert!(prefix_scope.is_validated(4));
        assert!(!prefix_scope.is_validated(5));
        assert_eq!(prefix_scope.scope.watermark(), 4);

        {
            let mut validated_object = ObjectParitySource::open(
                &mut source,
                sample_scheme(),
                TAPE_UUID,
                prefix_scope.clone(),
                BLOCK_SIZE,
                3,
                OpenTrust::RequireValidated,
            )
            .expect("object covered by the deeper prefix opens as validated");
            assert!(!validated_object.is_tar_only_unverified());

            let mut buf = vec![0u8; BLOCK_SIZE as usize];
            validated_object
                .read_block(&mut buf)
                .expect("clean read inside deeper prefix succeeds");
            assert_eq!(buf, block(0x20));

            let recovered = validated_object
                .recover_block_at(1)
                .expect("recovery inside deeper prefix uses the second sidecar");
            assert_eq!(recovered, block(0x21));
        }

        let rejected = match ObjectParitySource::open(
            &mut source,
            sample_scheme(),
            TAPE_UUID,
            prefix_scope.clone(),
            BLOCK_SIZE,
            5,
            OpenTrust::RequireValidated,
        ) {
            Err(err) => err,
            Ok(_) => panic!("require-validated rejects object beyond deeper prefix"),
        };
        assert!(matches!(
            rejected,
            ParityError::OutsideValidatedMapPrefix {
                ordinal: 4,
                prefix_ordinals: 4
            }
        ));

        let mut tar_only = ObjectParitySource::open(
            &mut source,
            sample_scheme(),
            TAPE_UUID,
            prefix_scope,
            BLOCK_SIZE,
            5,
            OpenTrust::AllowTarOnlyUnverified,
        )
        .expect("tar-only access remains available just outside deeper prefix");
        assert!(tar_only.is_tar_only_unverified());

        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        tar_only
            .read_block(&mut buf)
            .expect("clean tar-only suffix read still succeeds");
        assert_eq!(buf, block(0x30));

        let err = tar_only
            .recover_block_at(0)
            .expect_err("suffix parity recovery remains fenced by prefix scope");
        assert!(matches!(
            err,
            ParityError::OutsideValidatedMapPrefix {
                ordinal: 4,
                prefix_ordinals: 4
            }
        ));
    }

    #[test]
    fn acquire_filemark_map_requires_bootstrap_digest_when_catalog_missing() {
        let (records, expected_map) = fixture_records(false, false);
        let mut payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);
        payload.filemark_map_digest = None;
        let mut source = RecordingRawSource::new(records);

        let err =
            acquire_filemark_map(&mut source, &payload, None).expect_err("digest is required");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("does not carry a filemark-map digest"),
                    "{message}"
                );
            }
            other => panic!("expected filemark map error, got {other:?}"),
        }
        assert!(source.calls.is_empty());
    }

    #[test]
    fn damaged_bootstrap_header_becomes_object_candidate_and_digest_mismatches() {
        let (records, expected_map) = fixture_records(true, false);
        let final_digest = expected_map.digest(true).unwrap();
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_ne!(reconstructed, expected_map);
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(matches!(err, ParityError::FilemarkMapDigestMismatch));
    }

    #[test]
    fn bootstrap_wrong_declared_block_size_becomes_object_candidate_and_digest_mismatches() {
        let (mut records, expected_map) = fixture_records(false, false);
        let prefix_map =
            FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)]).expect("prefix validates");
        let mut wrong_size_payload = bootstrap_payload(prefix_map.digest(false).unwrap(), 0);
        wrong_size_payload.block_size_bytes = BLOCK_SIZE - 1;
        let mut wrong_size_bootstrap = vec![0u8; BLOCK_SIZE as usize];
        write_bootstrap_block(&wrong_size_payload, &mut wrong_size_bootstrap)
            .expect("wrong-size bootstrap still has internally valid CRCs");

        match records
            .first_mut()
            .expect("fixture starts with BOT bootstrap")
        {
            Record::Block(block) => *block = wrong_size_bootstrap,
            Record::Filemark => panic!("fixture must not start with a filemark"),
            Record::Unreadable => panic!("fixture must not start with an unreadable record"),
        }
        let Record::Block(bot_block) = records.first().unwrap() else {
            unreachable!("fixture starts with a block");
        };
        assert!(has_bootstrap_magic(bot_block));
        assert!(
            parse_bootstrap_block(bot_block).is_ok(),
            "test bootstrap should fail only scan block-size classification"
        );
        let final_digest = expected_map.digest(true).unwrap();
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_eq!(
            reconstructed
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                TapeFileKind::Object,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
            "valid bootstrap magic and CRCs are not enough when block_size_bytes disagrees with the scan block size"
        );
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(matches!(err, ParityError::FilemarkMapDigestMismatch));
    }

    #[test]
    fn bootstrap_payload_crc_failure_becomes_object_candidate_and_digest_mismatches() {
        let (mut records, expected_map) = fixture_records(false, false);
        let final_digest = expected_map.digest(true).unwrap();
        let block = records
            .iter_mut()
            .rev()
            .find_map(|record| match record {
                Record::Block(block) if has_bootstrap_magic(block) => Some(block),
                Record::Block(_) | Record::Filemark | Record::Unreadable => None,
            })
            .expect("fixture carries a final bootstrap block");
        corrupt_bootstrap_payload_crc(block);
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_ne!(reconstructed, expected_map);
        assert_eq!(
            reconstructed
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Object,
            ],
            "matching bootstrap magic plus a valid header is not enough; payload CRC failure must leave the file as an object candidate"
        );
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(matches!(err, ParityError::FilemarkMapDigestMismatch));
    }

    #[test]
    fn bootstrap_cbor_payload_failure_becomes_object_candidate_and_digest_mismatches() {
        let (mut records, expected_map) = fixture_records(false, false);
        let final_digest = expected_map.digest(true).unwrap();
        let block = records
            .iter_mut()
            .rev()
            .find_map(|record| match record {
                Record::Block(block) if has_bootstrap_magic(block) => Some(block),
                Record::Block(_) | Record::Filemark | Record::Unreadable => None,
            })
            .expect("fixture carries a final bootstrap block");
        corrupt_bootstrap_cbor_payload_with_valid_crc(block);
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_ne!(reconstructed, expected_map);
        assert_eq!(
            reconstructed
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Object,
            ],
            "final bootstrap magic plus valid CRCs are not enough; unparseable CBOR must leave the tape file as an object candidate"
        );
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(matches!(err, ParityError::FilemarkMapDigestMismatch));
    }

    #[test]
    fn bot_bootstrap_payload_crc_failure_becomes_object_candidate_and_digest_mismatches() {
        let (mut records, expected_map) = fixture_records(false, false);
        let final_digest = expected_map.digest(true).unwrap();
        match records
            .first_mut()
            .expect("fixture starts with the BOT bootstrap block")
        {
            Record::Block(block) => corrupt_bootstrap_payload_crc(block),
            Record::Filemark => panic!("fixture must not start with a filemark"),
            Record::Unreadable => panic!("fixture must not start with an unreadable record"),
        }
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_ne!(reconstructed, expected_map);
        assert_eq!(
            reconstructed
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                TapeFileKind::Object,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
            "BOT bootstrap magic plus a valid header is not enough; payload CRC failure must leave the file as an object candidate"
        );
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(matches!(err, ParityError::FilemarkMapDigestMismatch));
    }

    #[test]
    fn bot_bootstrap_cbor_payload_failure_becomes_object_candidate_and_digest_mismatches() {
        let (mut records, expected_map) = fixture_records(false, false);
        let final_digest = expected_map.digest(true).unwrap();
        match records
            .first_mut()
            .expect("fixture starts with the BOT bootstrap block")
        {
            Record::Block(block) => corrupt_bootstrap_cbor_payload_with_valid_crc(block),
            Record::Filemark => panic!("fixture must not start with a filemark"),
            Record::Unreadable => panic!("fixture must not start with an unreadable record"),
        }
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_ne!(reconstructed, expected_map);
        assert_eq!(
            reconstructed
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                TapeFileKind::Object,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
            "BOT bootstrap magic plus valid CRCs are not enough; unparseable CBOR must leave the tape file as an object candidate"
        );
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(matches!(err, ParityError::FilemarkMapDigestMismatch));
    }

    #[test]
    fn intermediate_bootstrap_payload_crc_failure_becomes_object_candidate_and_digest_mismatches() {
        let (mut records, expected_map, intermediate_bootstrap_tape_file) =
            fixture_records_with_intermediate_bootstrap();
        let final_digest = expected_map.digest(true).unwrap();
        let intermediate_position = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: intermediate_bootstrap_tape_file,
                block_within_file: 0,
            })
            .expect("intermediate bootstrap has a physical position");
        match records
            .get_mut(intermediate_position.lba as usize)
            .expect("fixture carries the intermediate bootstrap record")
        {
            Record::Block(block) => corrupt_bootstrap_payload_crc(block),
            Record::Filemark => panic!("intermediate bootstrap record must not be a filemark"),
            Record::Unreadable => {
                panic!("intermediate bootstrap record must not be unreadable")
            }
        }
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_ne!(reconstructed, expected_map);
        assert_eq!(
            reconstructed
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Object,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
            "intermediate bootstrap magic plus a valid header is not enough; payload CRC failure must leave the tape file as an object candidate"
        );
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(matches!(err, ParityError::FilemarkMapDigestMismatch));
    }

    #[test]
    fn intermediate_bootstrap_cbor_payload_failure_becomes_object_candidate_and_digest_mismatches()
    {
        let (mut records, expected_map, intermediate_bootstrap_tape_file) =
            fixture_records_with_intermediate_bootstrap();
        let final_digest = expected_map.digest(true).unwrap();
        let intermediate_position = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: intermediate_bootstrap_tape_file,
                block_within_file: 0,
            })
            .expect("intermediate bootstrap has a physical position");
        match records
            .get_mut(intermediate_position.lba as usize)
            .expect("fixture carries the intermediate bootstrap record")
        {
            Record::Block(block) => corrupt_bootstrap_cbor_payload_with_valid_crc(block),
            Record::Filemark => panic!("intermediate bootstrap record must not be a filemark"),
            Record::Unreadable => {
                panic!("intermediate bootstrap record must not be unreadable")
            }
        }
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_ne!(reconstructed, expected_map);
        assert_eq!(
            reconstructed
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Object,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
            "intermediate bootstrap magic plus valid CRCs are not enough; unparseable CBOR must leave the tape file as an object candidate"
        );
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(matches!(err, ParityError::FilemarkMapDigestMismatch));
    }

    #[test]
    fn damaged_final_bootstrap_header_without_trailing_filemark_is_structural_damage() {
        let (mut records, _expected_map) = fixture_records(false, false);
        assert!(matches!(records.pop(), Some(Record::Filemark)));
        let final_bootstrap_lba = records
            .len()
            .checked_sub(1)
            .expect("fixture keeps the final bootstrap block");
        match records
            .last_mut()
            .expect("fixture keeps the final bootstrap block")
        {
            Record::Block(block) => block[BOOTSTRAP_HEADER_CRC_OFFSET] ^= 0xFF,
            Record::Filemark => panic!("fixture final data record must be a bootstrap block"),
            Record::Unreadable => {
                panic!("fixture final data record must not be unreadable")
            }
        }
        let mut source = RecordingRawSource::new(records);

        let err = scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect_err("damaged final bootstrap tail without a filemark is structural damage");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(message.contains("missing a trailing filemark"), "{message}");
                assert!(
                    message.contains(&format!("physical LBA {final_bootstrap_lba}")),
                    "{message}"
                );
            }
            other => panic!("expected filemark-map reconstruction error, got {other:?}"),
        }
    }

    #[test]
    fn valid_bootstrap_with_extra_block_is_structural_damage() {
        let (mut records, expected_map) = fixture_records(false, false);
        let final_bootstrap = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: 3,
                block_within_file: 0,
            })
            .expect("final bootstrap has a physical position");
        let final_filemark_index = usize::try_from(final_bootstrap.lba + 1)
            .expect("final bootstrap filemark index fits usize");
        assert!(matches!(
            records.get(final_filemark_index),
            Some(Record::Filemark)
        ));
        records.insert(final_filemark_index, Record::Block(block(0xEE)));
        let mut source = RecordingRawSource::new(records);

        let err = scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect_err("a parseable bootstrap must still be a one-block tape file");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("bootstrap tape file has block_count 2, expected 1"),
                    "{message}"
                );
            }
            other => panic!("expected filemark-map reconstruction error, got {other:?}"),
        }
    }

    #[test]
    fn object_with_truncated_file_is_candidate_and_digest_mismatches() {
        let (mut records, expected_map) = fixture_records(false, false);
        let object = &expected_map.entries()[1];
        assert_eq!(object.kind, TapeFileKind::Object);
        assert!(
            object.block_count > 1,
            "fixture object must have a removable trailing block"
        );
        let last_object_block = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: object.tape_file_number,
                block_within_file: object.block_count - 1,
            })
            .expect("object last block has a physical position");
        let removed = records
            .remove(usize::try_from(last_object_block.lba).expect("object block index fits usize"));
        assert!(matches!(removed, Record::Block(_)));
        let final_digest = expected_map.digest(true).unwrap();
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_ne!(reconstructed, expected_map);
        assert_eq!(reconstructed.entries().len(), expected_map.entries().len());
        assert_eq!(
            reconstructed
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
            "object length drift must not alter catalog-less classification of neighboring structural files"
        );
        assert_eq!(
            reconstructed.entries()[1].block_count,
            object.block_count - 1
        );
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(
            matches!(err, ParityError::FilemarkMapDigestMismatch),
            "object length drift must be rejected by the final bootstrap digest, not by scan classification"
        );
    }

    #[test]
    fn object_with_extra_block_is_candidate_and_digest_mismatches() {
        let (mut records, expected_map) = fixture_records(false, false);
        let object = &expected_map.entries()[1];
        assert_eq!(object.kind, TapeFileKind::Object);
        assert!(
            object.block_count > 0,
            "fixture object must have a trailing filemark target"
        );
        let last_object_block = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: object.tape_file_number,
                block_within_file: object.block_count - 1,
            })
            .expect("object last block has a physical position");
        let object_filemark_index =
            usize::try_from(last_object_block.lba + 1).expect("object filemark index fits usize");
        assert!(matches!(
            records.get(object_filemark_index),
            Some(Record::Filemark)
        ));
        records.insert(object_filemark_index, Record::Block(block(0xEF)));
        let final_digest = expected_map.digest(true).unwrap();
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_ne!(reconstructed, expected_map);
        assert_eq!(reconstructed.entries().len(), expected_map.entries().len());
        assert_eq!(
            reconstructed
                .entries()
                .iter()
                .map(|entry| entry.kind)
                .collect::<Vec<_>>(),
            vec![
                TapeFileKind::Bootstrap,
                TapeFileKind::Object,
                TapeFileKind::ParitySidecar,
                TapeFileKind::Bootstrap,
            ],
            "object length drift must not alter catalog-less classification of neighboring structural files"
        );
        assert_eq!(
            reconstructed.entries()[1].block_count,
            object.block_count + 1
        );
        let err =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap_err();
        assert!(
            matches!(err, ParityError::FilemarkMapDigestMismatch),
            "object length drift must be rejected by the final bootstrap digest, not by scan classification"
        );
    }

    #[test]
    fn multi_epoch_object_extra_blocks_are_digest_checked_at_non_first_slots() {
        for object_tape_file in [3, 5] {
            let (mut records, expected_map) = multi_epoch_fixture_records(None);
            let object = expected_map
                .entries()
                .iter()
                .find(|entry| entry.tape_file_number == object_tape_file)
                .expect("fixture has requested object tape file");
            assert_eq!(object.kind, TapeFileKind::Object);
            let last_object_block = expected_map
                .physical_position(TapeFilePosition {
                    tape_file_number: object.tape_file_number,
                    block_within_file: object.block_count - 1,
                })
                .expect("object last block has a physical position");
            let object_filemark_index = usize::try_from(last_object_block.lba + 1)
                .expect("object filemark index fits usize");
            assert!(matches!(
                records.get(object_filemark_index),
                Some(Record::Filemark)
            ));
            records.insert(
                object_filemark_index,
                Record::Block(block(object_tape_file as u8 + 0x80)),
            );
            records.insert(
                object_filemark_index,
                Record::Block(block(object_tape_file as u8 + 0x70)),
            );
            let final_digest = expected_map.digest(true).unwrap();
            let mut source = RecordingRawSource::new(records);

            let reconstructed =
                scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

            assert_ne!(reconstructed, expected_map);
            assert_eq!(reconstructed.entries().len(), expected_map.entries().len());
            assert_eq!(
                reconstructed
                    .entries()
                    .iter()
                    .map(|entry| entry.kind)
                    .collect::<Vec<_>>(),
                vec![
                    TapeFileKind::Bootstrap,
                    TapeFileKind::Object,
                    TapeFileKind::ParitySidecar,
                    TapeFileKind::Object,
                    TapeFileKind::ParitySidecar,
                    TapeFileKind::Object,
                    TapeFileKind::ParitySidecar,
                    TapeFileKind::Bootstrap,
                ],
                "object length drift in tape file {object_tape_file} must not alter neighboring structural classifications"
            );
            let reconstructed_object = reconstructed
                .entries()
                .iter()
                .find(|entry| entry.tape_file_number == object_tape_file)
                .expect("reconstructed map keeps the object tape file");
            assert_eq!(reconstructed_object.block_count, object.block_count + 2);
            let err = ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest)
                .unwrap_err();
            assert!(
                matches!(err, ParityError::FilemarkMapDigestMismatch),
                "multi-block object length drift in tape file {object_tape_file} must be rejected by the final bootstrap digest"
            );
        }
    }

    #[test]
    fn object_first_block_with_structural_magic_but_invalid_crc_stays_object_candidate() {
        enum ObjectMagicCase {
            Bootstrap,
            Sidecar,
        }

        for case in [ObjectMagicCase::Bootstrap, ObjectMagicCase::Sidecar] {
            let (mut records, expected_map) = fixture_records(false, false);
            let object_lba = expected_map
                .physical_position(TapeFilePosition {
                    tape_file_number: 1,
                    block_within_file: 0,
                })
                .expect("object first block has a physical position")
                .lba;
            let object_index = usize::try_from(object_lba).expect("object LBA fits usize");
            let replacement = match case {
                ObjectMagicCase::Bootstrap => {
                    let prefix_map = FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])
                        .expect("prefix validates");
                    let mut block = bootstrap_block(prefix_map.digest(false).unwrap(), 99);
                    assert!(
                        has_bootstrap_magic(&block),
                        "test replacement must carry bootstrap magic"
                    );
                    block[BOOTSTRAP_HEADER_CRC_OFFSET] ^= 0xFF;
                    assert!(
                        parse_bootstrap_block(&block).is_err(),
                        "test replacement must fail bootstrap validation after magic"
                    );
                    block
                }
                ObjectMagicCase::Sidecar => {
                    let sidecar_lba = expected_map
                        .physical_position(TapeFilePosition {
                            tape_file_number: 2,
                            block_within_file: 0,
                        })
                        .expect("sidecar header has a physical position")
                        .lba;
                    let Record::Block(mut block) = records
                        [usize::try_from(sidecar_lba).expect("sidecar LBA fits usize")]
                    .clone() else {
                        panic!("sidecar header fixture record must be a block");
                    };
                    block[SIDECAR_HEADER_CRC_OFFSET] ^= 0xFF;
                    assert!(
                        matches!(
                            classify_sidecar_header_block(&block, &TAPE_UUID),
                            Err(ParityError::SidecarParse(_))
                        ),
                        "test replacement must match sidecar magic but fail header validation"
                    );
                    block
                }
            };
            records[object_index] = Record::Block(replacement);
            let final_digest = expected_map.digest(true).unwrap();
            let mut source = RecordingRawSource::new(records);

            let reconstructed =
                scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

            assert_eq!(
                reconstructed, expected_map,
                "object block content that only mimics structural magic must not become a bootstrap or sidecar"
            );
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest)
                .expect("filemark-map digest excludes object payload bytes");
        }
    }

    #[test]
    fn valid_sidecar_header_with_truncated_file_is_structural_damage() {
        let (mut records, expected_map) = fixture_records(false, false);
        let sidecar = &expected_map.entries()[2];
        assert_eq!(sidecar.kind, TapeFileKind::ParitySidecar);
        assert!(
            sidecar.block_count > 1,
            "fixture sidecar must have a removable trailing block"
        );
        let last_sidecar_block = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: sidecar.tape_file_number,
                block_within_file: sidecar.block_count - 1,
            })
            .expect("sidecar last block has a physical position");
        let removed = records.remove(
            usize::try_from(last_sidecar_block.lba).expect("sidecar block index fits usize"),
        );
        assert!(matches!(removed, Record::Block(_)));
        let mut source = RecordingRawSource::new(records);

        let err = scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect_err("a valid sidecar header must match the measured tape-file length");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("sidecar epoch 0 has block_count"),
                    "{message}"
                );
                assert!(
                    message.contains(&format!("expected {}", sidecar.block_count)),
                    "{message}"
                );
            }
            other => panic!("expected filemark-map reconstruction error, got {other:?}"),
        }
    }

    #[test]
    fn valid_sidecar_header_with_extra_block_is_structural_damage() {
        let (mut records, expected_map) = fixture_records(false, false);
        let sidecar = &expected_map.entries()[2];
        assert_eq!(sidecar.kind, TapeFileKind::ParitySidecar);
        assert!(
            sidecar.block_count > 0,
            "fixture sidecar must have a trailing filemark target"
        );
        let last_sidecar_block = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: sidecar.tape_file_number,
                block_within_file: sidecar.block_count - 1,
            })
            .expect("sidecar last block has a physical position");
        let sidecar_filemark_index =
            usize::try_from(last_sidecar_block.lba + 1).expect("sidecar filemark index fits usize");
        assert!(matches!(
            records.get(sidecar_filemark_index),
            Some(Record::Filemark)
        ));
        records.insert(sidecar_filemark_index, Record::Block(block(0xDD)));
        let mut source = RecordingRawSource::new(records);

        let err = scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect_err("a valid sidecar header must reject extra physical blocks");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(
                    message.contains("sidecar epoch 0 has block_count"),
                    "{message}"
                );
                assert!(
                    message.contains(&format!(
                        "block_count {}, expected {}",
                        sidecar.block_count + 1,
                        sidecar.block_count
                    )),
                    "{message}"
                );
            }
            other => panic!("expected filemark-map reconstruction error, got {other:?}"),
        }
    }

    #[test]
    fn damaged_sidecar_primary_header_uses_footer_tail_and_digest_validates() {
        let (records, expected_map) = fixture_records(false, true);
        let final_digest = expected_map.digest(true).unwrap();
        let mut source = RecordingRawSource::new(records);

        let reconstructed =
            scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE).unwrap();

        assert_eq!(reconstructed, expected_map);
        let scoped =
            ScopedFilemarkMap::validate_against_digest(reconstructed, &final_digest).unwrap();
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);
    }

    #[test]
    fn catalog_less_map_uses_tail_copy_when_sidecar_primary_header_is_destroyed() {
        let (records, expected_map) = fixture_records(false, true);
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);
        let mut source = RecordingRawSource::new(records);

        let scoped = acquire_filemark_map(&mut source, &final_payload, None)
            .expect("tail sidecar metadata copy preserves catalog-less map acquisition");
        assert_eq!(scoped.map, expected_map);
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert_eq!(scoped.scope.watermark(), 2);
    }

    #[test]
    fn catalog_less_scan_uses_tail_copy_for_corrupt_intermediate_sidecar_header() {
        let (records, expected_map) = multi_epoch_fixture_records(Some(1));
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);

        let mut scan_source = RecordingRawSource::new(records.clone());
        let reconstructed = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("CRC-bad intermediate sidecar remains structurally walkable");
        assert_eq!(reconstructed, expected_map);
        ScopedFilemarkMap::validate_against_digest(
            reconstructed,
            &expected_map.digest(true).unwrap(),
        )
        .expect("tail-copy classification preserves the final map digest");

        let mut source = RecordingRawSource::new(records);
        let scoped = acquire_filemark_map(&mut source, &final_payload, None)
            .expect("tail-copy scan supports catalog-less map acquisition");

        let recovered = recover_object_block_from_sidecar(
            &mut source,
            &scoped,
            &sample_scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            3,
            1,
        )
        .expect("catalog-less recovery uses the corrupted sidecar's tail metadata");

        assert_eq!(recovered.failed_ordinal, 3);
        assert_eq!(recovered.recovered_block, block(0x21));
        assert_eq!(recovered.sidecar_tape_file_number, 4);
    }

    #[test]
    fn catalog_present_recovery_succeeds_past_corrupt_intermediate_sidecar_header() {
        let (records, expected_map) = multi_epoch_fixture_records(Some(1));
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);
        let corrupt_sidecar_header_lba = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: 4,
                block_within_file: 0,
            })
            .expect("middle sidecar has a physical header position")
            .lba;
        let recovery_sidecar_header_lba = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: 6,
                block_within_file: 0,
            })
            .expect("later sidecar has a physical header position")
            .lba;

        let mut scan_source = RecordingRawSource::new(records.clone());
        let reconstructed = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("CRC-bad intermediate sidecar remains structurally walkable");
        assert_eq!(reconstructed, expected_map);
        ScopedFilemarkMap::validate_against_digest(
            reconstructed,
            &expected_map.digest(true).unwrap(),
        )
        .expect("scan uses the intermediate sidecar tail copy");

        let mut source = RecordingRawSource::new(records);
        let scoped = acquire_filemark_map(
            &mut source,
            &final_payload,
            Some(CatalogFilemarkMapInput::new(
                TAPE_UUID,
                expected_map.clone(),
                expected_map.max_sidecar_end_exclusive(),
            )),
        )
        .expect("catalog map bypasses the damaged intermediate scan path");
        assert!(source.calls.is_empty(), "catalog acquisition must not scan");

        let recovered = recover_object_block_from_sidecar(
            &mut source,
            &scoped,
            &sample_scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            5,
            1,
        )
        .expect("later epoch recovery uses the catalog map and healthy sidecar");

        assert_eq!(recovered.failed_ordinal, 5);
        assert_eq!(recovered.recovered_block, block(0x31));
        assert_eq!(recovered.sidecar_tape_file_number, 6);
        assert!(
            source
                .calls
                .contains(&ScanCall::Locate(recovery_sidecar_header_lba)),
            "recovery must read the healthy later sidecar selected by the catalog map"
        );
        assert!(
            !source
                .calls
                .contains(&ScanCall::Locate(corrupt_sidecar_header_lba)),
            "recovery of the later epoch must not touch the corrupted middle sidecar"
        );
    }

    #[test]
    fn catalog_less_scan_uses_tail_copy_for_corrupt_last_sidecar_header() {
        let (records, expected_map) = multi_epoch_fixture_records(Some(2));
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);

        let mut scan_source = RecordingRawSource::new(records.clone());
        let reconstructed = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("CRC-bad last sidecar remains structurally walkable");
        assert_eq!(reconstructed, expected_map);
        ScopedFilemarkMap::validate_against_digest(
            reconstructed,
            &expected_map.digest(true).unwrap(),
        )
        .expect("tail-copy classification preserves the final map digest");

        let mut source = RecordingRawSource::new(records);
        let scoped = acquire_filemark_map(&mut source, &final_payload, None)
            .expect("tail-copy scan supports catalog-less map acquisition");

        let recovered = recover_object_block_from_sidecar(
            &mut source,
            &scoped,
            &sample_scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            5,
            1,
        )
        .expect("catalog-less recovery uses the last sidecar's tail metadata");

        assert_eq!(recovered.failed_ordinal, 5);
        assert_eq!(recovered.recovered_block, block(0x31));
        assert_eq!(recovered.sidecar_tape_file_number, 6);
    }

    #[test]
    fn catalog_present_recovery_uses_tail_copy_for_corrupt_last_sidecar_header() {
        let (records, expected_map) = multi_epoch_fixture_records(Some(2));
        let final_payload = bootstrap_payload(expected_map.digest(true).unwrap(), 1);
        let watermark = expected_map.max_sidecar_end_exclusive();
        let healthy_sidecar_header_lba = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: 0,
            })
            .expect("first sidecar has a physical header position")
            .lba;
        let corrupt_sidecar_header_lba = expected_map
            .physical_position(TapeFilePosition {
                tape_file_number: 6,
                block_within_file: 0,
            })
            .expect("last sidecar has a physical header position")
            .lba;

        let mut scan_source = RecordingRawSource::new(records.clone());
        let reconstructed = scan_reconstruct_filemark_map(&mut scan_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("CRC-bad last sidecar remains structurally walkable");
        assert_eq!(reconstructed, expected_map);
        ScopedFilemarkMap::validate_against_digest(
            reconstructed,
            &expected_map.digest(true).unwrap(),
        )
        .expect("scan uses the last sidecar tail copy");

        let mut source = RecordingRawSource::new(records);
        let scoped = acquire_filemark_map(
            &mut source,
            &final_payload,
            Some(CatalogFilemarkMapInput::new(
                TAPE_UUID,
                expected_map.clone(),
                watermark,
            )),
        )
        .expect("catalog map bypasses the damaged last-sidecar scan path");
        assert!(source.calls.is_empty(), "catalog acquisition must not scan");

        let recovered = recover_object_block_from_sidecar(
            &mut source,
            &scoped,
            &sample_scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            1,
            1,
        )
        .expect("earlier epoch recovery uses a healthy sidecar");

        assert_eq!(recovered.failed_ordinal, 1);
        assert_eq!(recovered.recovered_block, block(0x11));
        assert_eq!(recovered.sidecar_tape_file_number, 2);
        assert!(
            source
                .calls
                .contains(&ScanCall::Locate(healthy_sidecar_header_lba)),
            "earlier recovery must read the healthy first sidecar"
        );
        assert!(
            !source
                .calls
                .contains(&ScanCall::Locate(corrupt_sidecar_header_lba)),
            "earlier recovery must not touch the corrupted last sidecar"
        );
        let calls_after_healthy_recovery = source.calls.len();

        let recovered = recover_object_block_from_sidecar(
            &mut source,
            &scoped,
            &sample_scheme(),
            TAPE_UUID,
            BLOCK_SIZE,
            5,
            1,
        )
        .expect("tail sidecar metadata copy should recover the corrupted last epoch");

        assert!(
            source.calls[calls_after_healthy_recovery..]
                .contains(&ScanCall::Locate(corrupt_sidecar_header_lba)),
            "corrupt-epoch recovery must follow the catalog pointer to the damaged sidecar"
        );
        assert_eq!(recovered.failed_ordinal, 5);
        assert_eq!(recovered.recovered_block, block(0x31));
        assert_eq!(recovered.sidecar_tape_file_number, 6);
    }

    #[test]
    fn damaged_sidecar_header_without_trailing_filemark_is_structural_damage() {
        let (mut records, _expected_map) = fixture_records(false, true);
        let removed_suffix = records.split_off(records.len() - 3);
        assert_eq!(
            removed_suffix.len(),
            3,
            "fixture shape must remove only the sidecar filemark and final bootstrap tape file"
        );
        assert!(matches!(removed_suffix.first(), Some(Record::Filemark)));
        assert!(matches!(removed_suffix.get(1), Some(Record::Block(_))));
        assert!(matches!(removed_suffix.get(2), Some(Record::Filemark)));
        let mut source = RecordingRawSource::new(records);

        let err = scan_reconstruct_filemark_map(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect_err("damaged sidecar tail without a filemark is structural damage");

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(message.contains("missing a trailing filemark"), "{message}");
                assert!(message.contains("physical LBA 5"), "{message}");
            }
            other => panic!("expected filemark-map reconstruction error, got {other:?}"),
        }
    }
}
