//! Semantic file rows and publication units, independent of host encoding.

use crate::mapping::data_shards_per_epoch;
use crate::{
    FilemarkMap, ObjectRecoveryRow, ParityError, ParityScheme, TapeFileKind, TapeFileMapEntry,
};
use serde::{Deserialize, Serialize};

/// One journaled tape-file row inside a committed bundle.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TapeFileEntry {
    /// Dense filemark-delimited tape-file number.
    pub tape_file_number: u64,
    /// Structural file kind.
    pub kind: TapeFileKind,
    /// Fixed-block count before the trailing filemark.
    pub block_count: u64,
    /// Advisory physical LOCATE hint. Map validation never trusts this alone.
    pub physical_start_hint: Option<u64>,
    /// Optional higher-layer object identifier. Layer 3c does not interpret it.
    pub object_id: Option<String>,
    /// First parity data ordinal for object tape files.
    pub first_parity_data_ordinal: Option<u64>,
    /// Epoch ID for parity-sidecar rows.
    pub epoch_id: Option<u64>,
    /// First protected ordinal for parity-sidecar rows.
    pub protected_ordinal_start: Option<u64>,
    /// End-exclusive protected ordinal for parity-sidecar rows.
    pub protected_ordinal_end_exclusive: Option<u64>,
    /// Canonical metadata hash for sidecar or parity_map control files.
    pub canonical_metadata_hash: Option<[u8; 32]>,
    /// Optional higher-layer recovery row for object tape files.
    pub object_recovery_row: Option<ObjectRecoveryRow>,
}

impl TapeFileEntry {
    /// Convert the structural part of a filemark-map row into a journal row.
    pub fn from_map_entry(entry: TapeFileMapEntry) -> Self {
        Self {
            tape_file_number: entry.tape_file_number,
            kind: entry.kind,
            block_count: entry.block_count,
            physical_start_hint: None,
            object_id: None,
            first_parity_data_ordinal: entry.first_parity_data_ordinal,
            epoch_id: entry.epoch_id,
            protected_ordinal_start: entry.protected_ordinal_start,
            protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        }
    }

    /// Return the structural filemark-map row represented by this journal row.
    pub fn to_map_entry(&self) -> TapeFileMapEntry {
        TapeFileMapEntry {
            tape_file_number: self.tape_file_number,
            kind: self.kind,
            block_count: self.block_count,
            first_parity_data_ordinal: self.first_parity_data_ordinal,
            protected_ordinal_start: self.protected_ordinal_start,
            protected_ordinal_end_exclusive: self.protected_ordinal_end_exclusive,
            epoch_id: self.epoch_id,
        }
    }
}

impl From<TapeFileMapEntry> for TapeFileEntry {
    fn from(entry: TapeFileMapEntry) -> Self {
        Self::from_map_entry(entry)
    }
}

/// Operational kind for one atomic committed bundle.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub enum CommittedBundleKind {
    /// Object tape file plus the sidecars it completed.
    Object,
    /// The sole tape-file-0 BOT Bootstrap.
    BotBootstrap,
    /// Sidecars emitted while closing a checkpoint epoch.
    CheckpointSidecars,
    /// Sidecars generated during restart append recovery.
    ResumeSidecars,
    /// Final partial sidecars and optional external ParityMap immediately
    /// before terminal replica A. This replacement grammar never includes a
    /// legacy final Bootstrap.
    TerminalPrefix,
    /// Exactly one barrier-proved A/gap-AB/B/gap-BC/C component.
    TerminalComponent,
    /// Watermark proving that all preceding bundles were projected by the
    /// shared checkpoint barrier.
    CheckpointedThrough,
}

/// One atomic journal commit unit.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CommittedBundle {
    /// Operational bundle kind.
    pub kind: CommittedBundleKind,
    /// Tape-file rows made durable by this commit, in ascending tape-file
    /// order.
    pub entries: Vec<TapeFileEntry>,
    /// Protection watermark after this bundle.
    pub highest_protected_ordinal: u64,
    /// Total committed object-data ordinals after this bundle.
    pub total_committed_ordinals: u64,
}

/// Structural grammar violation in one committed tape-file bundle.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind:?} committed bundle: {detail}")]
pub struct CommittedBundleShapeError {
    kind: CommittedBundleKind,
    detail: String,
}

impl CommittedBundleShapeError {
    fn new(kind: CommittedBundleKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

/// Validate the current-wire entry grammar for one committed bundle.
///
/// Object bundles contain their Object first followed only by completed
/// sidecars. The sole BOT Bootstrap, checkpoint sidecars, restart sidecars,
/// and terminal components each use distinct exact grammars.
pub fn validate_committed_bundle_shape(
    bundle: &CommittedBundle,
) -> Result<Option<&TapeFileEntry>, CommittedBundleShapeError> {
    validate_dense_entry_numbers(bundle)?;
    match bundle.kind {
        CommittedBundleKind::Object => {
            let Some((object, tail)) = bundle.entries.split_first() else {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain an Object entry",
                ));
            };
            if object.kind != TapeFileKind::Object {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    format!("must start with Object, got {:?}", object.kind),
                ));
            }
            validate_sidecar_prefix(bundle.kind, tail)?;
            Ok(None)
        }
        CommittedBundleKind::BotBootstrap => {
            let [entry] = bundle.entries.as_slice() else {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain exactly one Bootstrap",
                ));
            };
            if entry.tape_file_number != 0
                || entry.kind != TapeFileKind::Bootstrap
                || entry.block_count != 1
            {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain the one-block tape-file-0 BOT Bootstrap",
                ));
            }
            Ok(Some(entry))
        }
        CommittedBundleKind::TerminalPrefix => {
            validate_terminal_prefix(bundle.kind, &bundle.entries)?;
            Ok(None)
        }
        CommittedBundleKind::TerminalComponent => {
            if !validate_terminal_control_component(bundle.kind, &bundle.entries)? {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain exactly one terminal index or separation component",
                ));
            }
            Ok(None)
        }
        CommittedBundleKind::CheckpointSidecars | CommittedBundleKind::ResumeSidecars => {
            if bundle.entries.is_empty() {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must contain at least one ParitySidecar",
                ));
            }
            validate_sidecar_prefix(bundle.kind, &bundle.entries)?;
            Ok(None)
        }
        CommittedBundleKind::CheckpointedThrough => {
            if !bundle.entries.is_empty() {
                return Err(CommittedBundleShapeError::new(
                    bundle.kind,
                    "must not contain tape-file entries",
                ));
            }
            Ok(None)
        }
    }
}

fn validate_terminal_prefix(
    kind: CommittedBundleKind,
    entries: &[TapeFileEntry],
) -> Result<(), CommittedBundleShapeError> {
    let sidecars = match entries.split_last() {
        Some((last, sidecars)) if last.kind == TapeFileKind::ParityMap => sidecars,
        _ => entries,
    };
    validate_sidecar_prefix(kind, sidecars)
}

fn validate_terminal_control_component(
    kind: CommittedBundleKind,
    entries: &[TapeFileEntry],
) -> Result<bool, CommittedBundleShapeError> {
    let [entry] = entries else {
        return Ok(false);
    };
    if !matches!(
        entry.kind,
        TapeFileKind::TapeIndexReplica | TapeFileKind::IndexSeparationExtent
    ) {
        return Ok(false);
    }
    if entry.block_count == 0 {
        return Err(CommittedBundleShapeError::new(
            kind,
            format!(
                "terminal {:?} at tape file {} has zero blocks",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    if entry.object_id.is_some()
        || entry.first_parity_data_ordinal.is_some()
        || entry.epoch_id.is_some()
        || entry.protected_ordinal_start.is_some()
        || entry.protected_ordinal_end_exclusive.is_some()
        || entry.object_recovery_row.is_some()
    {
        return Err(CommittedBundleShapeError::new(
            kind,
            format!(
                "terminal {:?} at tape file {} has invalid kind-specific fields",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    Ok(true)
}

fn validate_dense_entry_numbers(bundle: &CommittedBundle) -> Result<(), CommittedBundleShapeError> {
    for pair in bundle.entries.windows(2) {
        let expected = pair[0].tape_file_number.checked_add(1).ok_or_else(|| {
            CommittedBundleShapeError::new(bundle.kind, "tape-file number overflows u64")
        })?;
        if pair[1].tape_file_number != expected {
            return Err(CommittedBundleShapeError::new(
                bundle.kind,
                format!(
                    "entries are not dense: expected tape file {expected}, got {}",
                    pair[1].tape_file_number
                ),
            ));
        }
    }
    Ok(())
}

fn validate_sidecar_prefix(
    kind: CommittedBundleKind,
    entries: &[TapeFileEntry],
) -> Result<(), CommittedBundleShapeError> {
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.kind != TapeFileKind::ParitySidecar)
    {
        return Err(CommittedBundleShapeError::new(
            kind,
            format!(
                "unexpected {:?} entry at tape file {}",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    Ok(())
}

/// Replay result for the committed prefix stored in a journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommittedState {
    /// Committed tape-file rows in ascending tape-file order.
    pub entries: Vec<TapeFileEntry>,
    /// Highest protected ordinal `W`.
    pub highest_protected_ordinal: u64,
    /// Total committed object-data ordinals `T`.
    pub total_committed_ordinals: u64,
    /// Valid bundles after the last checkpoint watermark. These records are
    /// excluded from `entries` and the committed watermarks.
    pub orphaned_bundles: Vec<CommittedBundle>,
}

impl CommittedState {
    /// Build the structural filemark map for this committed prefix.
    pub fn filemark_map(&self) -> Result<FilemarkMap, ParityError> {
        FilemarkMap::new(
            self.entries
                .iter()
                .map(TapeFileEntry::to_map_entry)
                .collect(),
        )
    }

    /// Validate the v0.7.2 restart invariant for a committed prefix.
    ///
    /// A full epoch auto-closes, so restart only has to rebuild the one open
    /// explicit range after `W`. This bound does not imply that `W` is aligned
    /// to `S*k`: a prior barrier may have closed a short epoch at any ordinal.
    /// If `T - W` reaches a full epoch, the auto-close invariant was violated
    /// and the journal must not be resumed in production mode.
    pub fn validate_v1_restart_bound(&self, scheme: &ParityScheme) -> Result<(), ParityError> {
        if self.highest_protected_ordinal > self.total_committed_ordinals {
            return Err(ParityError::ResumeAppend(format!(
                "journal committed state is incoherent: W={} exceeds T={}",
                self.highest_protected_ordinal, self.total_committed_ordinals
            )));
        }
        let live_ordinals = self
            .total_committed_ordinals
            .checked_sub(self.highest_protected_ordinal)
            .ok_or(ParityError::Invariant(
                "committed-state W/T arithmetic underflows",
            ))?;
        let epoch_data_shards = data_shards_per_epoch(scheme)?;
        if live_ordinals >= epoch_data_shards {
            return Err(ParityError::ResumeAppend(format!(
                "journal committed prefix has {live_ordinals} unprotected ordinals, \
                 exceeding the v1 restart bound of one partial epoch ({epoch_data_shards})"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod neutral_grammar_tests {
    use super::*;

    /// Neutral projection validation retains the codec's complete shape grammar
    /// and diagnostics across operation kinds, empty rows and hostile geometry.
    #[test]
    fn neutral_publication_grammar_preserves_legacy_refusals() {
        let kinds = [
            CommittedBundleKind::Object,
            CommittedBundleKind::BotBootstrap,
            CommittedBundleKind::ResumeSidecars,
            CommittedBundleKind::CheckpointSidecars,
            CommittedBundleKind::CheckpointedThrough,
            CommittedBundleKind::TerminalPrefix,
            CommittedBundleKind::TerminalComponent,
        ];
        for kind in kinds {
            for file_kind in [
                TapeFileKind::Object,
                TapeFileKind::Bootstrap,
                TapeFileKind::ParitySidecar,
                TapeFileKind::ParityMap,
                TapeFileKind::TapeIndexReplica,
                TapeFileKind::IndexSeparationExtent,
            ] {
                for count in 0..=2 {
                    for block_count in 0..=1 {
                        for gap in [0, 2] {
                            let entries = (0..count)
                                .map(|number| TapeFileEntry {
                                    tape_file_number: number + gap,
                                    kind: file_kind,
                                    block_count,
                                    physical_start_hint: None,
                                    object_id: None,
                                    first_parity_data_ordinal: None,
                                    epoch_id: None,
                                    protected_ordinal_start: None,
                                    protected_ordinal_end_exclusive: None,
                                    canonical_metadata_hash: None,
                                    object_recovery_row: None,
                                })
                                .collect();
                            let bundle = CommittedBundle {
                                kind,
                                entries,
                                highest_protected_ordinal: 0,
                                total_committed_ordinals: 0,
                            };
                            let old = validate_committed_bundle_shape(&bundle)
                                .map(|_| ())
                                .map_err(|e| e.to_string());
                            let neutral = crate::authority::FilePublication::from(&bundle)
                                .validate()
                                .map_err(|e| e.to_string());
                            assert_eq!(neutral, old, "{bundle:?}");
                        }
                    }
                }
            }
        }
    }
}
