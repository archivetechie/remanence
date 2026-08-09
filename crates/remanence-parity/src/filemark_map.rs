//! Filemark-map structures for Layer 3c v0.4.4.
//!
//! The filemark map is the structural catalog of physical tape files:
//! object archives and structural control files. It provides the
//! canonical SHA-256 projection used by BOT, catalog, and ParityMap authority,
//! plus local lookups from tape-file coordinates to parity
//! data ordinals and physical block-position hints.

use ciborium::value::Value as CborValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::ParityError;
use crate::parity_map::SidecarEpochDirectory;
use crate::raw::PhysicalPositionHint;

/// Digest metadata for validating a filemark map or a covered prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilemarkMapDigest {
    /// SHA-256 over the canonical map projection.
    pub map_sha256: [u8; 32],
    /// Number of leading tape files covered by this digest.
    pub tape_file_count: u64,
    /// Total object-data ordinals described by the covered prefix.
    pub map_total_data_ordinals: u64,
    /// Highest object-data ordinal protected by committed sidecars in the
    /// covered prefix.
    pub highest_protected_ordinal: u64,
    /// Whether this authority covers the complete supplied structural map.
    pub covers_complete_map: bool,
}

/// Logical tape-file address used by the filemark map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeFilePosition {
    /// Filemark-delimited tape file number, dense from BOT.
    pub tape_file_number: u64,
    /// Fixed-block offset inside that tape file.
    pub block_within_file: u64,
}

/// Kind discriminator for a filemark-map entry.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum TapeFileKind {
    /// Body-format object archive; every fixed block gets a
    /// `ParityDataOrdinal`.
    Object,
    /// Raw parity-sidecar tape file for one completed epoch.
    ParitySidecar,
    /// External sidecar epoch directory control tape file.
    ParityMap,
    /// Bootstrap tape file.
    Bootstrap,
    /// Complete terminal tape-index replica.
    TapeIndexReplica,
    /// Typed terminal index separation extent.
    IndexSeparationExtent,
}

impl TapeFileKind {
    fn projection_code(self) -> u64 {
        match self {
            Self::Object => 0,
            Self::ParitySidecar => 1,
            Self::Bootstrap => 2,
            Self::ParityMap => 3,
            Self::TapeIndexReplica => 4,
            Self::IndexSeparationExtent => 5,
        }
    }
}

/// One structural row of the Layer 3c filemark map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeFileMapEntry {
    /// Dense tape-file number from BOT.
    pub tape_file_number: u64,
    /// Tape-file kind.
    pub kind: TapeFileKind,
    /// Count of fixed-size data records before the trailing filemark.
    pub block_count: u64,
    /// First `ParityDataOrdinal` for object tape files.
    pub first_parity_data_ordinal: Option<u64>,
    /// First protected ordinal for parity sidecars.
    pub protected_ordinal_start: Option<u64>,
    /// End-exclusive protected ordinal for parity sidecars.
    pub protected_ordinal_end_exclusive: Option<u64>,
    /// Epoch ID for parity sidecars.
    pub epoch_id: Option<u64>,
}

impl TapeFileMapEntry {
    /// Construct an object tape-file entry.
    pub fn object(tape_file_number: u64, block_count: u64, first_parity_data_ordinal: u64) -> Self {
        Self {
            tape_file_number,
            kind: TapeFileKind::Object,
            block_count,
            first_parity_data_ordinal: Some(first_parity_data_ordinal),
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        }
    }

    /// Construct a parity-sidecar tape-file entry.
    pub fn parity_sidecar(
        tape_file_number: u64,
        block_count: u64,
        epoch_id: u64,
        protected_ordinal_start: u64,
        protected_ordinal_end_exclusive: u64,
    ) -> Self {
        Self {
            tape_file_number,
            kind: TapeFileKind::ParitySidecar,
            block_count,
            first_parity_data_ordinal: None,
            protected_ordinal_start: Some(protected_ordinal_start),
            protected_ordinal_end_exclusive: Some(protected_ordinal_end_exclusive),
            epoch_id: Some(epoch_id),
        }
    }

    /// Construct a bootstrap tape-file entry.
    pub fn bootstrap(tape_file_number: u64, block_count: u64) -> Self {
        Self {
            tape_file_number,
            kind: TapeFileKind::Bootstrap,
            block_count,
            first_parity_data_ordinal: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        }
    }

    /// Construct a parity-map control tape-file entry.
    pub fn parity_map(tape_file_number: u64, block_count: u64) -> Self {
        Self {
            tape_file_number,
            kind: TapeFileKind::ParityMap,
            block_count,
            first_parity_data_ordinal: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        }
    }

    /// Construct a terminal tape-index replica control-file entry.
    pub fn tape_index_replica(tape_file_number: u64, block_count: u64) -> Self {
        Self::structural_control(
            tape_file_number,
            TapeFileKind::TapeIndexReplica,
            block_count,
        )
    }

    /// Construct a typed terminal index separation-extent entry.
    pub fn index_separation_extent(tape_file_number: u64, block_count: u64) -> Self {
        Self::structural_control(
            tape_file_number,
            TapeFileKind::IndexSeparationExtent,
            block_count,
        )
    }

    fn structural_control(tape_file_number: u64, kind: TapeFileKind, block_count: u64) -> Self {
        Self {
            tape_file_number,
            kind,
            block_count,
            first_parity_data_ordinal: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            epoch_id: None,
        }
    }

    fn validate(&self) -> Result<(), ParityError> {
        if self.block_count == 0 {
            return Err(filemark_map_error(format!(
                "tape file {} has zero block_count",
                self.tape_file_number
            )));
        }

        match self.kind {
            TapeFileKind::Object => {
                if self.first_parity_data_ordinal.is_none()
                    || self.protected_ordinal_start.is_some()
                    || self.protected_ordinal_end_exclusive.is_some()
                    || self.epoch_id.is_some()
                {
                    return Err(filemark_map_error(format!(
                        "object tape file {} has invalid kind-specific fields",
                        self.tape_file_number
                    )));
                }
            }
            TapeFileKind::ParitySidecar => {
                let (Some(start), Some(end), Some(_epoch_id)) = (
                    self.protected_ordinal_start,
                    self.protected_ordinal_end_exclusive,
                    self.epoch_id,
                ) else {
                    return Err(filemark_map_error(format!(
                        "sidecar tape file {} is missing protected range or epoch_id",
                        self.tape_file_number
                    )));
                };
                if self.first_parity_data_ordinal.is_some() || end <= start {
                    return Err(filemark_map_error(format!(
                        "sidecar tape file {} has invalid kind-specific fields",
                        self.tape_file_number
                    )));
                }
            }
            TapeFileKind::Bootstrap => {
                if self.block_count != 1
                    || self.first_parity_data_ordinal.is_some()
                    || self.protected_ordinal_start.is_some()
                    || self.protected_ordinal_end_exclusive.is_some()
                    || self.epoch_id.is_some()
                {
                    return Err(filemark_map_error(format!(
                        "bootstrap tape file {} has invalid kind-specific fields",
                        self.tape_file_number
                    )));
                }
            }
            TapeFileKind::ParityMap
            | TapeFileKind::TapeIndexReplica
            | TapeFileKind::IndexSeparationExtent => {
                if self.first_parity_data_ordinal.is_some()
                    || self.protected_ordinal_start.is_some()
                    || self.protected_ordinal_end_exclusive.is_some()
                    || self.epoch_id.is_some()
                {
                    return Err(filemark_map_error(format!(
                        "structural control tape file {} has invalid kind-specific fields",
                        self.tape_file_number
                    )));
                }
            }
        }

        Ok(())
    }

    fn object_ordinal_end_exclusive(&self) -> Option<u64> {
        self.first_parity_data_ordinal?
            .checked_add(self.block_count)
    }
}

/// Complete structural filemark map for a tape or validated prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilemarkMap {
    entries: Vec<TapeFileMapEntry>,
    tape_file_count: u64,
}

impl FilemarkMap {
    /// Construct a validated map from already-numbered entries in ascending
    /// tape-file order.
    pub fn new(entries: Vec<TapeFileMapEntry>) -> Result<Self, ParityError> {
        let tape_file_count = checked_tape_file_count(entries.len())?;
        validate_entries(&entries)?;
        Ok(Self {
            entries,
            tape_file_count,
        })
    }

    /// Construct a validated map from catalog rows that may arrive in any
    /// query or insertion order.
    ///
    /// The canonical digest projection is defined over ascending
    /// `tape_file_number`, not over backend row order. This normalizes the
    /// rows before applying the same dense-numbering and kind-field
    /// validation as [`Self::new`].
    pub fn from_unordered_entries(mut entries: Vec<TapeFileMapEntry>) -> Result<Self, ParityError> {
        entries.sort_by_key(|entry| entry.tape_file_number);
        Self::new(entries)
    }

    /// Borrow the validated entries in ascending tape-file order.
    pub fn entries(&self) -> &[TapeFileMapEntry] {
        &self.entries
    }

    /// Number of tape files in the map.
    pub fn tape_file_count(&self) -> u64 {
        self.tape_file_count
    }

    /// Total object-data ordinals described by the map.
    pub fn total_data_ordinals(&self) -> u64 {
        self.entries
            .iter()
            .filter_map(TapeFileMapEntry::object_ordinal_end_exclusive)
            .max()
            .unwrap_or(0)
    }

    /// Highest ordinal protected by sidecars in the map.
    pub fn max_sidecar_end_exclusive(&self) -> u64 {
        self.entries
            .iter()
            .filter_map(|entry| entry.protected_ordinal_end_exclusive)
            .max()
            .unwrap_or(0)
    }

    /// Return a validated prefix containing the first `tape_file_count`
    /// entries.
    pub fn truncate_to_tape_files(&self, tape_file_count: u64) -> Result<Self, ParityError> {
        let end = usize::try_from(tape_file_count).map_err(|_| {
            filemark_map_error(format!(
                "prefix tape_file_count {tape_file_count} does not fit usize"
            ))
        })?;
        if end > self.entries.len() {
            return Err(filemark_map_error(format!(
                "prefix tape_file_count {tape_file_count} exceeds map length {}",
                self.entries.len()
            )));
        }
        Self::new(self.entries[..end].to_vec())
    }

    /// Canonical CBOR projection bytes used as the SHA-256 digest input.
    pub fn canonical_projection_bytes(&self) -> Result<Vec<u8>, ParityError> {
        let projection = CborValue::Array(
            self.entries
                .iter()
                .map(|entry| {
                    CborValue::Array(vec![
                        CborValue::Integer(entry.tape_file_number.into()),
                        CborValue::Integer(entry.kind.projection_code().into()),
                        CborValue::Integer(entry.block_count.into()),
                        optional_u64(entry.first_parity_data_ordinal),
                        optional_u64(entry.protected_ordinal_start),
                        optional_u64(entry.protected_ordinal_end_exclusive),
                        optional_u64(entry.epoch_id),
                    ])
                })
                .collect(),
        );
        let mut bytes = Vec::new();
        ciborium::into_writer(&projection, &mut bytes)
            .map_err(|e| filemark_map_error(format!("canonical CBOR encode failed: {e}")))?;
        Ok(bytes)
    }

    /// SHA-256 over [`Self::canonical_projection_bytes`].
    pub fn canonical_digest(&self) -> Result<[u8; 32], ParityError> {
        let bytes = self.canonical_projection_bytes()?;
        let digest = Sha256::digest(&bytes);
        Ok(digest.into())
    }

    /// Build digest metadata for this map or prefix.
    pub fn digest(&self, covers_complete_map: bool) -> Result<FilemarkMapDigest, ParityError> {
        Ok(FilemarkMapDigest {
            map_sha256: self.canonical_digest()?,
            tape_file_count: self.tape_file_count(),
            map_total_data_ordinals: self.total_data_ordinals(),
            highest_protected_ordinal: self.max_sidecar_end_exclusive(),
            covers_complete_map,
        })
    }

    /// Translate a tape-file block position to a parity data ordinal.
    /// Returns `Ok(None)` for non-object tape files.
    pub fn ordinal_at(&self, position: TapeFilePosition) -> Result<Option<u64>, ParityError> {
        let entry = self.entry(position.tape_file_number)?;
        if position.block_within_file >= entry.block_count {
            return Err(filemark_map_error(format!(
                "block {} is outside tape file {} with block_count {}",
                position.block_within_file, position.tape_file_number, entry.block_count
            )));
        }
        match (entry.kind, entry.first_parity_data_ordinal) {
            (TapeFileKind::Object, Some(first)) => Ok(Some(first + position.block_within_file)),
            _ => Ok(None),
        }
    }

    /// Find the object tape-file position for a parity data ordinal.
    pub fn position_for_ordinal(&self, ordinal: u64) -> Result<TapeFilePosition, ParityError> {
        for entry in &self.entries {
            let Some(first) = entry.first_parity_data_ordinal else {
                continue;
            };
            let Some(end) = first.checked_add(entry.block_count) else {
                return Err(filemark_map_error(format!(
                    "object tape file {} ordinal range overflows",
                    entry.tape_file_number
                )));
            };
            if ordinal >= first && ordinal < end {
                return Ok(TapeFilePosition {
                    tape_file_number: entry.tape_file_number,
                    block_within_file: ordinal - first,
                });
            }
        }
        Err(filemark_map_error(format!(
            "ordinal {ordinal} is not described by the filemark map"
        )))
    }

    /// Convert a tape-file position to a physical block-position hint by
    /// summing preceding tape-file records plus their trailing filemarks.
    pub fn physical_position(
        &self,
        position: TapeFilePosition,
    ) -> Result<PhysicalPositionHint, ParityError> {
        let entry = self.entry(position.tape_file_number)?;
        if position.block_within_file >= entry.block_count {
            return Err(filemark_map_error(format!(
                "block {} is outside tape file {} with block_count {}",
                position.block_within_file, position.tape_file_number, entry.block_count
            )));
        }

        let tape_file_index = usize::try_from(position.tape_file_number).map_err(|_| {
            filemark_map_error(format!(
                "tape file {} does not fit the host lookup domain",
                position.tape_file_number
            ))
        })?;
        let prefix_blocks_and_filemarks =
            self.entries[..tape_file_index]
                .iter()
                .try_fold(0u64, |acc, prior| {
                    acc.checked_add(prior.block_count)
                        .and_then(|value| value.checked_add(1))
                        .ok_or_else(|| filemark_map_error("physical position overflows u64"))
                })?;
        let lba = prefix_blocks_and_filemarks
            .checked_add(position.block_within_file)
            .ok_or_else(|| filemark_map_error("physical position overflows u64"))?;
        Ok(PhysicalPositionHint::new(lba))
    }

    /// Return the physical append point immediately after this committed
    /// prefix's last trailing filemark.
    ///
    /// Restart/append uses this as the catalog-derived position where the next
    /// write supersedes any uncommitted physical tail.
    pub fn append_position_after_prefix(&self) -> Result<PhysicalPositionHint, ParityError> {
        let lba = self.entries.iter().try_fold(0u64, |acc, entry| {
            acc.checked_add(entry.block_count)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| filemark_map_error("append position overflows u64"))
        })?;
        Ok(PhysicalPositionHint::new(lba))
    }

    fn entry(&self, tape_file_number: u64) -> Result<&TapeFileMapEntry, ParityError> {
        let index = usize::try_from(tape_file_number).map_err(|_| {
            filemark_map_error(format!(
                "tape file {tape_file_number} does not fit the host lookup domain"
            ))
        })?;
        self.entries
            .get(index)
            .filter(|entry| entry.tape_file_number == tape_file_number)
            .ok_or_else(|| {
                filemark_map_error(format!(
                    "tape file {tape_file_number} is not described by the filemark map"
                ))
            })
    }
}

/// Canonical key-2 digest carried by the sole parity-enabled BOT Bootstrap.
pub fn sole_bot_filemark_map_digest() -> Result<FilemarkMapDigest, ParityError> {
    FilemarkMap::new(vec![TapeFileMapEntry::bootstrap(0, 1)])?.digest(false)
}

fn checked_tape_file_count(entry_count: usize) -> Result<u64, ParityError> {
    u64::try_from(entry_count).map_err(|_| filemark_map_error("tape file count exceeds u64::MAX"))
}

/// Incremental builder used by writers and tests as tape files are emitted.
#[derive(Clone, Debug, Default)]
pub struct FilemarkMapBuilder {
    entries: Vec<TapeFileMapEntry>,
    base_tape_file_count: u64,
    base_total_data_ordinals: u64,
}

impl FilemarkMapBuilder {
    /// Create an empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Borrow the in-progress entries in ascending tape-file order.
    pub fn entries(&self) -> &[TapeFileMapEntry] {
        &self.entries
    }

    /// Total object-data ordinals described by the in-progress map.
    pub fn total_data_ordinals(&self) -> Result<u64, ParityError> {
        self.current_total_data_ordinals()
    }

    /// Rebuild a builder from a validated committed prefix.
    ///
    /// Restart/append uses this after it has verified the catalog prefix and
    /// positioned just past the last committed tape file; the next push then
    /// naturally assigns the first post-prefix tape-file number.
    pub fn from_committed_prefix(prefix: &FilemarkMap) -> Self {
        Self {
            entries: prefix.entries.clone(),
            base_tape_file_count: 0,
            base_total_data_ordinals: 0,
        }
    }

    /// Seed an append-only builder from bounded replay scalars.
    pub fn from_bounded_prefix(tape_file_count: u64, total_data_ordinals: u64) -> Self {
        Self {
            entries: Vec::new(),
            base_tape_file_count: tape_file_count,
            base_total_data_ordinals: total_data_ordinals,
        }
    }

    /// Total prefix plus live-session structural row count.
    pub fn tape_file_count(&self) -> Result<u64, ParityError> {
        self.base_tape_file_count
            .checked_add(checked_tape_file_count(self.entries.len())?)
            .ok_or_else(|| filemark_map_error("tape file count overflows u64"))
    }

    /// Borrow live-session entries beginning at an absolute tape-file number.
    pub fn session_entries_from(
        &self,
        tape_file_number: u64,
    ) -> Result<&[TapeFileMapEntry], ParityError> {
        let offset = tape_file_number
            .checked_sub(self.base_tape_file_count)
            .ok_or_else(|| filemark_map_error("requested row precedes bounded session base"))?;
        let offset = usize::try_from(offset)
            .map_err(|_| filemark_map_error("session row offset does not fit usize"))?;
        self.entries
            .get(offset..)
            .ok_or_else(|| filemark_map_error("session row offset exceeds live rows"))
    }

    /// Append an object tape file and return its assigned map entry.
    pub fn push_object(&mut self, block_count: u64) -> Result<TapeFileMapEntry, ParityError> {
        self.require_bot_authority()?;
        let tape_file_number = self.next_tape_file_number()?;
        let first_ordinal = self.current_total_data_ordinals()?;
        let entry = TapeFileMapEntry::object(tape_file_number, block_count, first_ordinal);
        entry.validate()?;
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// Append a parity-sidecar tape file and return its assigned map entry.
    pub fn push_parity_sidecar(
        &mut self,
        block_count: u64,
        epoch_id: u64,
        protected_ordinal_start: u64,
        protected_ordinal_end_exclusive: u64,
    ) -> Result<TapeFileMapEntry, ParityError> {
        self.require_bot_authority()?;
        let tape_file_number = self.next_tape_file_number()?;
        let entry = TapeFileMapEntry::parity_sidecar(
            tape_file_number,
            block_count,
            epoch_id,
            protected_ordinal_start,
            protected_ordinal_end_exclusive,
        );
        entry.validate()?;
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// Append the sole tape-file-0 BOT Bootstrap map entry.
    pub fn push_bootstrap(&mut self) -> Result<TapeFileMapEntry, ParityError> {
        if self.base_tape_file_count != 0 || !self.entries.is_empty() {
            return Err(filemark_map_error(
                "schema-major 2 permits a Bootstrap only as tape file 0",
            ));
        }
        let tape_file_number = self.next_tape_file_number()?;
        let entry = TapeFileMapEntry::bootstrap(tape_file_number, 1);
        entry.validate()?;
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// Append a parity-map control tape file and return its assigned map entry.
    pub fn push_parity_map(&mut self, block_count: u64) -> Result<TapeFileMapEntry, ParityError> {
        self.require_bot_authority()?;
        let tape_file_number = self.next_tape_file_number()?;
        let entry = TapeFileMapEntry::parity_map(tape_file_number, block_count);
        entry.validate()?;
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// Append a terminal tape-index replica and return its assigned map entry.
    pub fn push_tape_index_replica(
        &mut self,
        block_count: u64,
    ) -> Result<TapeFileMapEntry, ParityError> {
        self.require_bot_authority()?;
        let tape_file_number = self.next_tape_file_number()?;
        let entry = TapeFileMapEntry::tape_index_replica(tape_file_number, block_count);
        entry.validate()?;
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// Append a typed terminal index separation extent.
    pub fn push_index_separation_extent(
        &mut self,
        block_count: u64,
    ) -> Result<TapeFileMapEntry, ParityError> {
        self.require_bot_authority()?;
        let tape_file_number = self.next_tape_file_number()?;
        let entry = TapeFileMapEntry::index_separation_extent(tape_file_number, block_count);
        entry.validate()?;
        self.entries.push(entry.clone());
        Ok(entry)
    }

    /// Compute the digest for the current map plus already-numbered
    /// provisional entries without mutating this builder.
    ///
    /// Writer code uses this for control files whose payload must commit to a
    /// map scope before the filemark/catalog barrier has made those rows
    /// durable.
    pub fn projected_digest(
        &self,
        provisional_entries: &[TapeFileMapEntry],
        covers_complete_map: bool,
    ) -> Result<FilemarkMapDigest, ParityError> {
        if self.base_tape_file_count != 0 {
            return Err(filemark_map_error(
                "bounded-prefix digest requires journal replay authority",
            ));
        }
        let mut entries = self.entries.clone();
        entries.extend_from_slice(provisional_entries);
        FilemarkMap::new(entries)?.digest(covers_complete_map)
    }

    /// Finish into a validated [`FilemarkMap`].
    pub fn build(self) -> Result<FilemarkMap, ParityError> {
        FilemarkMap::new(self.entries)
    }

    /// Next tape-file number that would be assigned by an append operation.
    pub fn next_tape_file_number(&self) -> Result<u64, ParityError> {
        self.tape_file_count()
    }

    fn current_total_data_ordinals(&self) -> Result<u64, ParityError> {
        self.entries
            .iter()
            .try_fold(self.base_total_data_ordinals, |total, entry| {
                if entry.kind == TapeFileKind::Object {
                    total.checked_add(entry.block_count).ok_or_else(|| {
                        filemark_map_error("total data ordinals overflow while building map")
                    })
                } else {
                    Ok(total)
                }
            })
    }

    fn require_bot_authority(&self) -> Result<(), ParityError> {
        if self.base_tape_file_count > 0 {
            return Ok(());
        }
        if matches!(
            self.entries.first(),
            Some(entry)
                if entry.tape_file_number == 0
                    && entry.kind == TapeFileKind::Bootstrap
                    && entry.block_count == 1
        ) {
            Ok(())
        } else {
            Err(filemark_map_error(
                "schema-major 2 writers must commit the sole tape-file-0 BOT Bootstrap before body files",
            ))
        }
    }
}

/// Filemark map plus its digest-checked recovery scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopedFilemarkMap {
    /// Full catalog or scan-reconstructed map. In the prefix case, suffix
    /// entries are navigational only and not authoritative.
    pub map: FilemarkMap,
    /// `None` means the full map is validated; `Some(n)` means only tape
    /// files `0..n` were validated by the selected digest authority.
    pub validated_prefix_tape_files: Option<u64>,
    /// Recovery scope carried by the selected catalog or on-media authority.
    pub scope: MapScope,
    /// Authoritative sidecar epoch directory, when one was available from a
    /// ParityMap. REM-PARITY 13.3 step 3 uses it to locate and validate a
    /// sidecar's tail metadata copy when both the primary header and the footer
    /// are lost; without it that rescue is not possible and the epoch is
    /// declared metadata-unavailable.
    pub sidecar_directory: Option<SidecarEpochDirectory>,
}

impl ScopedFilemarkMap {
    /// Construct a complete scoped map from the catalog and its protection
    /// watermark.
    pub fn from_catalog(map: FilemarkMap, highest_protected_ordinal: u64) -> Self {
        Self {
            map,
            validated_prefix_tape_files: None,
            scope: MapScope::Complete {
                highest_protected_ordinal,
            },
            sidecar_directory: None,
        }
    }

    /// Attach the authoritative sidecar epoch directory, enabling the
    /// REM-PARITY 13.3 step 3 tail rescue.
    #[must_use]
    pub fn with_sidecar_directory(mut self, directory: Option<SidecarEpochDirectory>) -> Self {
        self.sidecar_directory = directory;
        self
    }

    /// Validate a scan-reconstructed map against digest authority.
    pub fn validate_against_digest(
        full_map: FilemarkMap,
        digest: &FilemarkMapDigest,
    ) -> Result<Self, ParityError> {
        let validated = if digest.covers_complete_map {
            full_map.clone()
        } else {
            full_map.truncate_to_tape_files(digest.tape_file_count)?
        };

        if validated.canonical_digest()? != digest.map_sha256
            || validated.tape_file_count() != digest.tape_file_count
            || validated.total_data_ordinals() != digest.map_total_data_ordinals
            || validated.max_sidecar_end_exclusive() != digest.highest_protected_ordinal
        {
            return Err(ParityError::FilemarkMapDigestMismatch {
                truncation_position: None,
            });
        }

        let (validated_prefix_tape_files, scope) = if digest.covers_complete_map {
            (
                None,
                MapScope::Complete {
                    highest_protected_ordinal: digest.highest_protected_ordinal,
                },
            )
        } else {
            (
                Some(digest.tape_file_count),
                MapScope::Prefix {
                    map_total_data_ordinals: digest.map_total_data_ordinals,
                    highest_protected_ordinal: digest.highest_protected_ordinal,
                },
            )
        };

        Ok(Self {
            map: full_map,
            validated_prefix_tape_files,
            scope,
            sidecar_directory: None,
        })
    }

    /// Whether a tape-file number is inside the digest-checked prefix.
    pub fn is_validated(&self, tape_file_number: u64) -> bool {
        match self.validated_prefix_tape_files {
            None => true,
            Some(prefix) => tape_file_number < prefix,
        }
    }
}

/// Digest-checked recovery scope for a filemark map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapScope {
    /// Whole-map scope from a catalog or complete ParityMap authority.
    Complete {
        /// Highest protected ordinal.
        highest_protected_ordinal: u64,
    },
    /// Prefix scope from a bounded digest authority.
    Prefix {
        /// Total object-data ordinals named by the digest-checked prefix.
        map_total_data_ordinals: u64,
        /// Highest protected ordinal.
        highest_protected_ordinal: u64,
    },
}

impl MapScope {
    /// Protection watermark in either scope arm.
    pub fn watermark(&self) -> u64 {
        match self {
            Self::Complete {
                highest_protected_ordinal,
            }
            | Self::Prefix {
                highest_protected_ordinal,
                ..
            } => *highest_protected_ordinal,
        }
    }

    /// Check whether a parity data ordinal may be recovered under this scope.
    pub fn recoverable(&self, ordinal: u64) -> Result<(), ParityError> {
        if let Self::Prefix {
            map_total_data_ordinals,
            ..
        } = self
        {
            if ordinal >= *map_total_data_ordinals {
                return Err(ParityError::OutsideValidatedMapPrefix {
                    ordinal,
                    prefix_ordinals: *map_total_data_ordinals,
                });
            }
        }

        let watermark = self.watermark();
        if ordinal >= watermark {
            return Err(ParityError::UnrecoverablePendingEpoch {
                failed_ordinal: ordinal,
                watermark,
            });
        }

        Ok(())
    }
}

fn validate_entries(entries: &[TapeFileMapEntry]) -> Result<(), ParityError> {
    let Some(bot) = entries.first() else {
        return Err(filemark_map_error(
            "schema-major 2 filemark maps require the sole tape-file-0 BOT Bootstrap",
        ));
    };
    if bot.tape_file_number != 0 || bot.kind != TapeFileKind::Bootstrap || bot.block_count != 1 {
        return Err(filemark_map_error(
            "schema-major 2 filemark maps require a one-block Bootstrap at tape file 0",
        ));
    }
    let mut next_object_ordinal = 0u64;
    for (index, entry) in entries.iter().enumerate() {
        let expected_file_number = u64::try_from(index)
            .map_err(|_| filemark_map_error("tape file count exceeds u64::MAX"))?;
        if entry.tape_file_number != expected_file_number {
            return Err(filemark_map_error(format!(
                "tape file numbers must be dense from 0: expected {expected_file_number}, got {}",
                entry.tape_file_number
            )));
        }

        entry.validate()?;
        if index != 0 && entry.kind == TapeFileKind::Bootstrap {
            return Err(filemark_map_error(
                "schema-major 2 filemark maps forbid Bootstrap outside tape file 0",
            ));
        }
        if entry.kind == TapeFileKind::Object {
            let first = entry
                .first_parity_data_ordinal
                .ok_or_else(|| filemark_map_error("object entry missing first ordinal"))?;
            if first != next_object_ordinal {
                return Err(filemark_map_error(format!(
                    "object tape file {} starts at ordinal {first}, expected {next_object_ordinal}",
                    entry.tape_file_number
                )));
            }
            next_object_ordinal = next_object_ordinal
                .checked_add(entry.block_count)
                .ok_or_else(|| filemark_map_error("total data ordinals overflow"))?;
        }
    }
    Ok(())
}

fn optional_u64(value: Option<u64>) -> CborValue {
    value.map_or(CborValue::Null, |value| CborValue::Integer(value.into()))
}

fn filemark_map_error(message: impl Into<String>) -> ParityError {
    ParityError::FilemarkMapReconstruct(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::{
        parse_bootstrap_block, write_bootstrap_block, BootstrapPayload, ParitySchemeRecord,
    };

    fn sample_map() -> FilemarkMap {
        FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 3, 0),
            TapeFileMapEntry::parity_sidecar(2, 2, 7, 0, 3),
        ])
        .expect("sample map validates")
    }

    #[test]
    fn canonical_projection_bytes_are_stable() {
        let map = sample_map();
        let bytes = map
            .canonical_projection_bytes()
            .expect("canonical projection");
        assert_eq!(
            hex(&bytes),
            "8387000201f6f6f6f68701000300f6f6f687020102f6000307"
        );
        assert_eq!(
            hex(&map.canonical_digest().expect("digest")),
            "548ca6c967073a6c1ad011d10fc132c2739e251d015ea45a628bbec96892c26b"
        );
    }

    #[test]
    fn canonical_projection_digest_is_stable_across_catalog_row_order() {
        let entries = vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 3, 0),
            TapeFileMapEntry::parity_sidecar(2, 2, 7, 0, 3),
            TapeFileMapEntry::object(3, 2, 3),
            TapeFileMapEntry::parity_map(4, 1),
        ];
        let ordered = FilemarkMap::new(entries.clone()).expect("ordered map validates");
        let unordered = FilemarkMap::from_unordered_entries(vec![
            entries[3].clone(),
            entries[1].clone(),
            entries[4].clone(),
            entries[0].clone(),
            entries[2].clone(),
        ])
        .expect("unordered catalog rows normalize");

        assert_eq!(unordered.entries(), ordered.entries());
        assert_eq!(
            unordered
                .canonical_projection_bytes()
                .expect("unordered projection"),
            ordered
                .canonical_projection_bytes()
                .expect("ordered projection")
        );
        assert_eq!(
            unordered.canonical_digest().expect("unordered digest"),
            ordered.canonical_digest().expect("ordered digest")
        );
    }

    #[test]
    fn unordered_catalog_rows_still_reject_non_dense_file_numbers() {
        let err = FilemarkMap::from_unordered_entries(vec![
            TapeFileMapEntry::object(2, 3, 0),
            TapeFileMapEntry::bootstrap(0, 1),
        ])
        .unwrap_err();

        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(message.contains("dense from 0"), "{message}");
            }
            other => panic!("expected filemark map error, got {other:?}"),
        }
    }

    #[test]
    fn map_requires_exactly_one_sole_bot_bootstrap() {
        let missing = FilemarkMap::new(vec![TapeFileMapEntry::object(0, 1, 0)])
            .expect_err("missing BOT Bootstrap must fail");
        assert!(missing.to_string().contains("one-block Bootstrap"));

        let nonzero = FilemarkMap::new(vec![
            TapeFileMapEntry::object(0, 1, 0),
            TapeFileMapEntry::bootstrap(1, 1),
        ])
        .expect_err("nonzero Bootstrap must fail");
        assert!(nonzero.to_string().contains("one-block Bootstrap"));

        let duplicate = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::bootstrap(1, 1),
        ])
        .expect_err("duplicate Bootstrap must fail");
        assert!(duplicate.to_string().contains("forbid Bootstrap"));
    }

    #[test]
    fn builder_assigns_object_ordinals_and_tape_file_numbers() {
        let mut builder = FilemarkMapBuilder::new();
        assert_eq!(builder.push_bootstrap().unwrap().tape_file_number, 0);
        let object_a = builder.push_object(3).unwrap();
        assert_eq!(object_a.tape_file_number, 1);
        assert_eq!(object_a.first_parity_data_ordinal, Some(0));
        let sidecar = builder.push_parity_sidecar(2, 7, 0, 3).unwrap();
        assert_eq!(sidecar.tape_file_number, 2);
        let object_b = builder.push_object(2).unwrap();
        assert_eq!(object_b.tape_file_number, 3);
        assert_eq!(object_b.first_parity_data_ordinal, Some(3));

        let map = builder.build().unwrap();
        assert_eq!(map.tape_file_count(), 4);
        assert_eq!(map.total_data_ordinals(), 5);
        assert_eq!(map.max_sidecar_end_exclusive(), 3);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn tape_file_count_preserves_values_beyond_u32() {
        let entry_count =
            usize::try_from(u64::from(u32::MAX) + 1).expect("64-bit usize represents u32::MAX + 1");
        assert_eq!(
            checked_tape_file_count(entry_count).expect("u64 count preserves the full usize value"),
            u64::from(u32::MAX) + 1
        );
    }

    #[test]
    fn terminal_structural_kinds_have_stable_codes_and_no_ordinal_fields() {
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::tape_index_replica(1, 3),
            TapeFileMapEntry::index_separation_extent(2, 4),
        ])
        .expect("terminal structural kinds validate");

        assert_eq!(TapeFileKind::TapeIndexReplica.projection_code(), 4);
        assert_eq!(TapeFileKind::IndexSeparationExtent.projection_code(), 5);
        for entry in &map.entries()[1..] {
            assert_eq!(entry.first_parity_data_ordinal, None);
            assert_eq!(entry.protected_ordinal_start, None);
            assert_eq!(entry.protected_ordinal_end_exclusive, None);
            assert_eq!(entry.epoch_id, None);
        }
        assert_eq!(map.tape_file_count(), 3);
        assert_eq!(
            map.ordinal_at(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: 0,
            })
            .expect("terminal control lookup"),
            None
        );

        let mut builder = FilemarkMapBuilder::new();
        builder.push_bootstrap().expect("BOT Bootstrap");
        assert_eq!(
            builder.push_tape_index_replica(3).expect("replica entry"),
            TapeFileMapEntry::tape_index_replica(1, 3)
        );
        assert_eq!(
            builder
                .push_index_separation_extent(4)
                .expect("separation entry"),
            TapeFileMapEntry::index_separation_extent(2, 4)
        );

        let mut invalid = TapeFileMapEntry::tape_index_replica(0, 2);
        invalid.first_parity_data_ordinal = Some(0);
        assert!(
            FilemarkMap::new(vec![invalid]).is_err(),
            "terminal structural kinds must reject Object ordinal fields"
        );
    }

    #[test]
    fn u64_tape_file_lookup_boundaries_fail_closed_without_narrowing() {
        let map = sample_map();
        for tape_file_number in [
            u64::from(u32::MAX) + 1,
            (1u64 << 53) + 1,
            i64::MAX as u64 + 1,
            u64::MAX,
        ] {
            let error = map
                .ordinal_at(TapeFilePosition {
                    tape_file_number,
                    block_within_file: 0,
                })
                .expect_err("out-of-map u64 file identity must reject");
            assert!(
                matches!(error, ParityError::FilemarkMapReconstruct(_)),
                "unexpected error for tape file {tape_file_number}: {error}"
            );
        }
    }

    #[test]
    fn parity_map_entries_are_structural_control_files_without_ordinals() {
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::parity_map(1, 3),
            TapeFileMapEntry::object(2, 2, 0),
            TapeFileMapEntry::parity_sidecar(3, 5, 0, 0, 2),
        ])
        .expect("parity-map structural entry validates");

        assert_eq!(map.total_data_ordinals(), 2);
        assert_eq!(map.max_sidecar_end_exclusive(), 2);
        assert_eq!(
            map.ordinal_at(TapeFilePosition {
                tape_file_number: 1,
                block_within_file: 0,
            })
            .unwrap(),
            None
        );

        let mut builder = FilemarkMapBuilder::new();
        builder.push_bootstrap().unwrap();
        let parity_map = builder.push_parity_map(3).unwrap();
        assert_eq!(parity_map, TapeFileMapEntry::parity_map(1, 3));
        assert_eq!(
            builder.push_object(2).unwrap().first_parity_data_ordinal,
            Some(0)
        );
    }

    #[test]
    fn builder_rebuilt_from_committed_prefix_continues_tape_file_numbers() {
        let prefix = sample_map();

        let mut builder = FilemarkMapBuilder::from_committed_prefix(&prefix);
        let next = builder.push_object(2).unwrap();

        assert_eq!(next.tape_file_number, 3);
        assert_eq!(next.first_parity_data_ordinal, Some(3));
    }

    #[test]
    fn sole_bot_builder_rejects_a_later_bootstrap() {
        let mut builder = FilemarkMapBuilder::new();
        builder.push_bootstrap().unwrap();
        builder.push_object(3).unwrap();
        let error = builder
            .push_bootstrap()
            .expect_err("a later Bootstrap must be rejected");
        assert!(error.to_string().contains("only as tape file 0"));
    }

    #[test]
    fn builder_requires_bot_before_body_and_bounded_append_cannot_add_bootstrap() {
        let mut fresh = FilemarkMapBuilder::new();
        for error in [
            fresh.push_object(1).unwrap_err(),
            fresh.push_parity_map(1).unwrap_err(),
            fresh.push_tape_index_replica(1).unwrap_err(),
        ] {
            assert!(error.to_string().contains("before body files"), "{error}");
        }

        let mut append = FilemarkMapBuilder::from_bounded_prefix(7, 11);
        let object = append
            .push_object(2)
            .expect("nonempty bounded prefix carries BOT authority");
        assert_eq!(object.tape_file_number, 7);
        assert_eq!(object.first_parity_data_ordinal, Some(11));
        let error = append
            .push_bootstrap()
            .expect_err("bounded append must never emit another Bootstrap");
        assert!(error.to_string().contains("only as tape file 0"), "{error}");
    }

    #[test]
    fn bootstrap_payload_bytes_do_not_feed_back_into_map_digest() {
        const BLOCK_SIZE: usize = 512;

        let mut builder = FilemarkMapBuilder::new();
        builder.push_bootstrap().unwrap();
        let map = builder.build().unwrap();
        let digest = map.digest(false).unwrap();

        let payload_a = bootstrap_payload(digest.clone(), 0, "writer-a", "2026-05-23T03:15:00Z");
        let payload_b = bootstrap_payload(
            digest.clone(),
            0,
            "writer-b-with-different-cbor",
            "2026-05-23T03:16:00Z",
        );
        let mut block_a = vec![0u8; BLOCK_SIZE];
        let mut block_b = vec![0u8; BLOCK_SIZE];

        write_bootstrap_block(&payload_a, &mut block_a).unwrap();
        write_bootstrap_block(&payload_b, &mut block_b).unwrap();

        assert_ne!(
            block_a, block_b,
            "different bootstrap header/CBOR payload bytes must not change the structural map digest"
        );

        let parsed_a = parse_bootstrap_block(&block_a).unwrap();
        let parsed_b = parse_bootstrap_block(&block_b).unwrap();
        assert_eq!(parsed_a.filemark_map_digest, Some(digest.clone()));
        assert_eq!(parsed_b.filemark_map_digest, Some(digest.clone()));
        assert_eq!(map.canonical_digest().unwrap(), digest.map_sha256);
    }

    #[test]
    fn open_epoch_digest_keeps_total_ordinals_distinct_from_watermark() {
        let map = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(1, 5, 0),
            TapeFileMapEntry::parity_sidecar(2, 2, 0, 0, 3),
            TapeFileMapEntry::parity_map(3, 1),
        ])
        .expect("open-epoch map validates");
        let digest = map.digest(true).unwrap();

        assert_eq!(digest.map_total_data_ordinals, 5);
        assert_eq!(digest.highest_protected_ordinal, 3);
        assert!(
            digest.highest_protected_ordinal < digest.map_total_data_ordinals,
            "object data can be on tape before its open epoch has a sidecar"
        );

        let scoped = ScopedFilemarkMap::validate_against_digest(map, &digest).unwrap();
        scoped.scope.recoverable(2).unwrap();
        let err = scoped.scope.recoverable(3).unwrap_err();
        assert!(matches!(
            err,
            ParityError::UnrecoverablePendingEpoch {
                failed_ordinal: 3,
                watermark: 3
            }
        ));
    }

    #[test]
    fn lookup_handles_object_sidecar_and_bootstrap_files() {
        let map = sample_map();
        assert_eq!(
            map.ordinal_at(TapeFilePosition {
                tape_file_number: 1,
                block_within_file: 2,
            })
            .unwrap(),
            Some(2)
        );
        assert_eq!(
            map.ordinal_at(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: 0,
            })
            .unwrap(),
            None
        );
        assert_eq!(
            map.ordinal_at(TapeFilePosition {
                tape_file_number: 0,
                block_within_file: 0,
            })
            .unwrap(),
            None
        );
        assert_eq!(
            map.position_for_ordinal(2).unwrap(),
            TapeFilePosition {
                tape_file_number: 1,
                block_within_file: 2,
            }
        );
    }

    #[test]
    fn physical_position_counts_trailing_filemarks_between_tape_files() {
        let map = sample_map();
        assert_eq!(
            map.physical_position(TapeFilePosition {
                tape_file_number: 0,
                block_within_file: 0,
            })
            .unwrap(),
            PhysicalPositionHint::new(0)
        );
        assert_eq!(
            map.physical_position(TapeFilePosition {
                tape_file_number: 1,
                block_within_file: 0,
            })
            .unwrap(),
            PhysicalPositionHint::new(2)
        );
        assert_eq!(
            map.physical_position(TapeFilePosition {
                tape_file_number: 2,
                block_within_file: 1,
            })
            .unwrap(),
            PhysicalPositionHint::new(7)
        );
    }

    #[test]
    fn validate_against_final_digest_requires_matching_projection_and_cross_checks() {
        let map = sample_map();
        let digest = map.digest(true).unwrap();
        let scoped = ScopedFilemarkMap::validate_against_digest(map.clone(), &digest).unwrap();
        assert_eq!(scoped.validated_prefix_tape_files, None);
        assert!(scoped.is_validated(99));
        assert_eq!(scoped.scope.watermark(), 3);

        let mut bad_digest = digest;
        bad_digest.map_total_data_ordinals += 1;
        let err = ScopedFilemarkMap::validate_against_digest(map, &bad_digest).unwrap_err();
        assert!(matches!(err, ParityError::FilemarkMapDigestMismatch { .. }));
    }

    #[test]
    fn validate_against_prefix_digest_bounds_recovery_to_prefix_and_watermark() {
        let mut builder = FilemarkMapBuilder::new();
        builder.push_bootstrap().unwrap();
        builder.push_object(3).unwrap();
        builder.push_parity_sidecar(2, 7, 0, 3).unwrap();
        let prefix = builder.clone().build().unwrap();
        builder.push_object(4).unwrap();
        let full = builder.build().unwrap();

        let digest = prefix.digest(false).unwrap();
        let scoped = ScopedFilemarkMap::validate_against_digest(full, &digest).unwrap();
        assert_eq!(scoped.validated_prefix_tape_files, Some(3));
        assert!(scoped.is_validated(2));
        assert!(!scoped.is_validated(3));
        scoped.scope.recoverable(2).unwrap();

        let err = scoped.scope.recoverable(3).unwrap_err();
        assert!(matches!(
            err,
            ParityError::OutsideValidatedMapPrefix {
                ordinal: 3,
                prefix_ordinals: 3
            }
        ));
    }

    #[test]
    fn prefix_scope_distinguishes_named_but_unprotected_ordinals() {
        let scope = MapScope::Prefix {
            map_total_data_ordinals: 10,
            highest_protected_ordinal: 6,
        };
        scope.recoverable(5).unwrap();
        let err = scope.recoverable(6).unwrap_err();
        assert!(matches!(
            err,
            ParityError::UnrecoverablePendingEpoch {
                failed_ordinal: 6,
                watermark: 6
            }
        ));
    }

    #[test]
    fn map_rejects_non_dense_tape_file_numbers() {
        let err = FilemarkMap::new(vec![
            TapeFileMapEntry::bootstrap(0, 1),
            TapeFileMapEntry::object(2, 3, 0),
        ])
        .unwrap_err();
        match err {
            ParityError::FilemarkMapReconstruct(message) => {
                assert!(message.contains("dense from 0"), "{message}");
            }
            other => panic!("expected filemark map error, got {other:?}"),
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn bootstrap_payload(
        digest: FilemarkMapDigest,
        sequence: u64,
        written_by_version: &str,
        written_at: &str,
    ) -> BootstrapPayload {
        BootstrapPayload {
            scheme: Some(ParitySchemeRecord {
                id: "test-rs".to_string(),
                data_blocks_per_stripe: 2,
                parity_blocks_per_stripe: 1,
                stripes_per_neighborhood: 1,
                no_parity_flag: false,
            }),
            no_parity_flag: false,
            filemark_map_digest: Some(digest),
            tape_uuid: [0x42; 16],
            written_by_version: written_by_version.to_string(),
            written_at: written_at.to_string(),
            sequence,
            block_size_bytes: 512,
            drive_compression: false,
        }
    }
}
