//! Semantic file rows and publication units, independent of host encoding.

#[cfg(any(test, feature = "fixtures"))]
use crate::{mapping::data_shards_per_epoch, FilemarkMap, ParityError, ParityScheme};
use crate::{ObjectRecoveryRow, TapeFileKind, TapeFileMapEntry};
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
pub enum FileRowsKind {
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
    /// Test input delimiter closing a native commit group; never stored as a row.
    #[cfg(any(test, feature = "fixtures"))]
    FixtureBoundary,
}

/// One atomic journal commit unit.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct FileRowsReport {
    /// Operational bundle kind.
    pub kind: FileRowsKind,
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
pub struct FileRowsShapeError {
    kind: FileRowsKind,
    detail: String,
}

impl FileRowsShapeError {
    fn new(kind: FileRowsKind, detail: impl Into<String>) -> Self {
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
    bundle: &FileRowsReport,
) -> Result<Option<&TapeFileEntry>, FileRowsShapeError> {
    validate_dense_entry_numbers(bundle)?;
    match bundle.kind {
        FileRowsKind::Object => {
            let Some((object, tail)) = bundle.entries.split_first() else {
                return Err(FileRowsShapeError::new(
                    bundle.kind,
                    "must contain an Object entry",
                ));
            };
            if object.kind != TapeFileKind::Object {
                return Err(FileRowsShapeError::new(
                    bundle.kind,
                    format!("must start with Object, got {:?}", object.kind),
                ));
            }
            validate_sidecar_prefix(bundle.kind, tail)?;
            Ok(None)
        }
        FileRowsKind::BotBootstrap => {
            let [entry] = bundle.entries.as_slice() else {
                return Err(FileRowsShapeError::new(
                    bundle.kind,
                    "must contain exactly one Bootstrap",
                ));
            };
            if entry.tape_file_number != 0
                || entry.kind != TapeFileKind::Bootstrap
                || entry.block_count != 1
            {
                return Err(FileRowsShapeError::new(
                    bundle.kind,
                    "must contain the one-block tape-file-0 BOT Bootstrap",
                ));
            }
            Ok(Some(entry))
        }
        FileRowsKind::TerminalPrefix => {
            validate_terminal_prefix(bundle.kind, &bundle.entries)?;
            Ok(None)
        }
        FileRowsKind::TerminalComponent => {
            if !validate_terminal_control_component(bundle.kind, &bundle.entries)? {
                return Err(FileRowsShapeError::new(
                    bundle.kind,
                    "must contain exactly one terminal index or separation component",
                ));
            }
            Ok(None)
        }
        FileRowsKind::CheckpointSidecars | FileRowsKind::ResumeSidecars => {
            if bundle.entries.is_empty() {
                return Err(FileRowsShapeError::new(
                    bundle.kind,
                    "must contain at least one ParitySidecar",
                ));
            }
            validate_sidecar_prefix(bundle.kind, &bundle.entries)?;
            Ok(None)
        }
        #[cfg(any(test, feature = "fixtures"))]
        FileRowsKind::FixtureBoundary => {
            if !bundle.entries.is_empty() {
                return Err(FileRowsShapeError::new(
                    bundle.kind,
                    "must not contain tape-file entries",
                ));
            }
            Ok(None)
        }
    }
}

fn validate_terminal_prefix(
    kind: FileRowsKind,
    entries: &[TapeFileEntry],
) -> Result<(), FileRowsShapeError> {
    let sidecars = match entries.split_last() {
        Some((last, sidecars)) if last.kind == TapeFileKind::ParityMap => sidecars,
        _ => entries,
    };
    validate_sidecar_prefix(kind, sidecars)
}

fn validate_terminal_control_component(
    kind: FileRowsKind,
    entries: &[TapeFileEntry],
) -> Result<bool, FileRowsShapeError> {
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
        return Err(FileRowsShapeError::new(
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
        return Err(FileRowsShapeError::new(
            kind,
            format!(
                "terminal {:?} at tape file {} has invalid kind-specific fields",
                entry.kind, entry.tape_file_number
            ),
        ));
    }
    Ok(true)
}

fn validate_dense_entry_numbers(bundle: &FileRowsReport) -> Result<(), FileRowsShapeError> {
    for pair in bundle.entries.windows(2) {
        let expected = pair[0].tape_file_number.checked_add(1).ok_or_else(|| {
            FileRowsShapeError::new(bundle.kind, "tape-file number overflows u64")
        })?;
        if pair[1].tape_file_number != expected {
            return Err(FileRowsShapeError::new(
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
    kind: FileRowsKind,
    entries: &[TapeFileEntry],
) -> Result<(), FileRowsShapeError> {
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.kind != TapeFileKind::ParitySidecar)
    {
        return Err(FileRowsShapeError::new(
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
#[cfg(any(test, feature = "fixtures"))]
pub struct FileRowsState {
    /// Committed tape-file rows in ascending tape-file order.
    pub entries: Vec<TapeFileEntry>,
    /// Highest protected ordinal `W`.
    pub highest_protected_ordinal: u64,
    /// Total committed object-data ordinals `T`.
    pub total_committed_ordinals: u64,
    /// Valid bundles after the last checkpoint watermark. These records are
    /// excluded from `entries` and the committed watermarks.
    pub orphaned_bundles: Vec<FileRowsReport>,
}

#[cfg(any(test, feature = "fixtures"))]
impl FileRowsState {
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
            FileRowsKind::Object,
            FileRowsKind::BotBootstrap,
            FileRowsKind::ResumeSidecars,
            FileRowsKind::CheckpointSidecars,
            FileRowsKind::FixtureBoundary,
            FileRowsKind::TerminalPrefix,
            FileRowsKind::TerminalComponent,
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
                            let bundle = FileRowsReport {
                                kind,
                                entries,
                                highest_protected_ordinal: 0,
                                total_committed_ordinals: 0,
                            };
                            let old = validate_committed_bundle_shape(&bundle)
                                .map(|_| ())
                                .map_err(|e| e.to_string());
                            let physical = crate::authority::FilePublication::try_from(&bundle);
                            if kind == FileRowsKind::FixtureBoundary {
                                assert!(
                                    physical.is_err(),
                                    "host watermark is not a file publication"
                                );
                            } else {
                                let neutral = physical.unwrap().validate();
                                assert_eq!(neutral.map_err(|e| e.to_string()), old, "{bundle:?}");
                            }
                        }
                    }
                }
            }
        }
    }
}

pub use crate::authority::FrozenFileRows;
/// Durable-append journal failures from Layer 3c v0.7.2 §10.2/§10.6.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// Authority is durable but a subsequent fold/projection step failed.
    #[error("journal authority is durable; replay is required: {0}")]
    AfterDurable(String),
    /// An old-format publication phase has no single-log equivalent.
    #[error("authority operation requires B3b migration: {0}")]
    UnsupportedOperation(&'static str),
    /// Underlying filesystem I/O failed.
    #[error("journal I/O: {0}")]
    Io(#[from] std::io::Error),
    /// Existing journal header does not match the tape UUID, block size,
    /// compression-off precondition, or parity scheme requested by this
    /// session.
    #[error("journal header mismatch (tape_uuid / scheme / block_size / drive_compression)")]
    HeaderMismatch,
    /// Journal record encoding or decoding failed.
    #[error("journal encode/decode: {0}")]
    Codec(String),
    /// A bundle does not match the structural grammar for its operational
    /// kind.
    #[error(transparent)]
    InvalidBundleShape(#[from] FileRowsShapeError),
    /// Journal path is on a filesystem class that cannot be trusted as a
    /// crash-recovery commit point.
    #[error(
        "journal volume rejected: {0} (must be a trusted local volume with honored fsync; §10.6)"
    )]
    UntrustedVolume(String),
    /// Replay found a torn or corrupt tail that must be reconciled against the
    /// physical tape before any append may proceed.
    #[error("journal recovery required: {0}")]
    RecoveryRequired(String),
}

impl JournalError {
    /// True when a non-blocking journal lock could not be acquired because
    /// another append or replay handle already owns the per-tape journal.
    pub fn is_lock_contended(&self) -> bool {
        matches!(self, JournalError::Io(err) if err.kind() == std::io::ErrorKind::WouldBlock)
    }
}

/// Deterministic allocation and pass counters for bounded journal replay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BoundedJournalReplayMetrics {
    /// Complete validation scans performed before row replay.
    pub validation_passes: u64,
    /// Complete/started row-emission scans performed by this replay handle.
    pub row_replay_passes: u64,
    /// Number of framed bundle records seen by validation.
    pub journal_record_count: u64,
    /// Largest validated encoded bundle payload in bytes.
    pub peak_record_payload_bytes: u64,
    /// Largest number of decoded tape-file rows simultaneously retained by
    /// the validation scan. This includes the prior final-candidate bundle.
    pub peak_live_entry_count: u64,
}

/// Exact agreement with the native terminal component prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalComponentAuthorityRelation {
    /// All proved rows match the immutable component plan and progress.
    Aligned,
}
impl From<remanence_library::durability::VolumeError> for JournalError {
    fn from(error: remanence_library::durability::VolumeError) -> Self {
        match error {
            remanence_library::durability::VolumeError::Io(error) => Self::Io(error),
            remanence_library::durability::VolumeError::UntrustedVolume(detail) => {
                Self::UntrustedVolume(detail)
            }
        }
    }
}

impl TryFrom<&FileRowsReport> for crate::authority::FilePublication {
    type Error = JournalError;
    fn try_from(bundle: &FileRowsReport) -> Result<Self, Self::Error> {
        use crate::authority::FilePurpose as New;
        use crate::FileRowsKind as Old;
        Ok(Self {
            purpose: match bundle.kind {
                Old::BotBootstrap => New::Bootstrap,
                Old::Object => New::Object,
                Old::ResumeSidecars => {
                    New::Sidecars(crate::authority::SidecarProvenance::Reconstructed)
                }
                Old::CheckpointSidecars => {
                    New::Sidecars(crate::authority::SidecarProvenance::Fresh)
                }
                #[cfg(any(test, feature = "fixtures"))]
                Old::FixtureBoundary => {
                    return Err(JournalError::UnsupportedOperation(
                        "legacy watermark is not a physical file publication",
                    ))
                }
                Old::TerminalPrefix => New::TerminalPrefix,
                Old::TerminalComponent => New::TerminalComponent,
            },
            files: bundle.entries.clone(),
            protected: bundle.highest_protected_ordinal,
            total: bundle.total_committed_ordinals,
            cursor: None,
            check: None,
            seal_due: None,
        })
    }
}
impl From<&crate::authority::FilePublication> for FileRowsReport {
    fn from(publication: &crate::authority::FilePublication) -> Self {
        use crate::authority::FilePurpose as New;
        use crate::FileRowsKind as Old;
        Self {
            kind: match publication.purpose {
                New::Bootstrap => Old::BotBootstrap,
                New::Object => Old::Object,
                New::Sidecars(crate::authority::SidecarProvenance::Reconstructed) => {
                    Old::ResumeSidecars
                }
                New::Sidecars(crate::authority::SidecarProvenance::Fresh) => {
                    Old::CheckpointSidecars
                }
                New::TerminalPrefix => Old::TerminalPrefix,
                New::TerminalComponent => Old::TerminalComponent,
            },
            entries: publication.files.clone(),
            highest_protected_ordinal: publication.protected,
            total_committed_ordinals: publication.total,
        }
    }
}

#[cfg(any(test, feature = "fixtures"))]
impl From<&FileRowsState> for crate::authority::StructuralState {
    fn from(state: &FileRowsState) -> Self {
        Self {
            files: state.entries.clone(),
            protected: state.highest_protected_ordinal,
            total: state.total_committed_ordinals,
            pending: state
                .orphaned_bundles
                .iter()
                .filter_map(|b| crate::authority::FilePublication::try_from(b).ok())
                .collect(),
        }
    }
}
#[cfg(any(test, feature = "fixtures"))]
impl From<&crate::authority::StructuralState> for FileRowsState {
    fn from(state: &crate::authority::StructuralState) -> Self {
        Self {
            entries: state.files.clone(),
            highest_protected_ordinal: state.protected,
            total_committed_ordinals: state.total,
            orphaned_bundles: state.pending.iter().map(Into::into).collect(),
        }
    }
}

#[cfg(test)]
mod moved_tests {
    use super::*;
    use crate::SchemeId;
    fn sample_bundle() -> FileRowsReport {
        FileRowsReport {
            kind: FileRowsKind::Object,
            entries: vec![
                {
                    let mut entry =
                        TapeFileEntry::from_map_entry(TapeFileMapEntry::object(0, 3, 0));
                    entry.object_recovery_row = Some(
                        crate::ObjectRecoveryRow::encrypted(
                            0,
                            3,
                            vec![[0x24; 16], [0x25; 16]],
                            66,
                            2377,
                        )
                        .with_object_id([0x44; 16]),
                    );
                    entry
                },
                TapeFileEntry {
                    canonical_metadata_hash: Some([0x5A; 32]),
                    ..TapeFileEntry::from_map_entry(TapeFileMapEntry::parity_sidecar(1, 9, 0, 0, 3))
                },
            ],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        }
    }

    fn structural_entry(tape_file_number: u64, kind: TapeFileKind) -> TapeFileEntry {
        TapeFileEntry {
            tape_file_number,
            kind,
            block_count: if kind == TapeFileKind::Bootstrap {
                1
            } else {
                2
            },
            physical_start_hint: None,
            object_id: (kind == TapeFileKind::Object).then(|| "object-1".to_string()),
            first_parity_data_ordinal: (kind == TapeFileKind::Object).then_some(0),
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        }
    }

    fn structural_bundle(kind: FileRowsKind, entry_kinds: &[TapeFileKind]) -> FileRowsReport {
        FileRowsReport {
            kind,
            entries: entry_kinds
                .iter()
                .enumerate()
                .map(|(number, kind)| {
                    structural_entry(u64::try_from(number).expect("test index fits u64"), *kind)
                })
                .collect(),
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        }
    }

    #[test]
    fn shared_bundle_validator_accepts_every_current_wire_shape() {
        let accepted = [
            structural_bundle(FileRowsKind::Object, &[TapeFileKind::Object]),
            structural_bundle(
                FileRowsKind::Object,
                &[TapeFileKind::Object, TapeFileKind::ParitySidecar],
            ),
            structural_bundle(FileRowsKind::BotBootstrap, &[TapeFileKind::Bootstrap]),
            structural_bundle(
                FileRowsKind::CheckpointSidecars,
                &[TapeFileKind::ParitySidecar],
            ),
            structural_bundle(
                FileRowsKind::ResumeSidecars,
                &[TapeFileKind::ParitySidecar, TapeFileKind::ParitySidecar],
            ),
            structural_bundle(
                FileRowsKind::TerminalPrefix,
                &[TapeFileKind::ParitySidecar, TapeFileKind::ParityMap],
            ),
            structural_bundle(FileRowsKind::TerminalPrefix, &[]),
            structural_bundle(
                FileRowsKind::TerminalComponent,
                &[TapeFileKind::TapeIndexReplica],
            ),
            structural_bundle(
                FileRowsKind::TerminalComponent,
                &[TapeFileKind::IndexSeparationExtent],
            ),
            structural_bundle(FileRowsKind::FixtureBoundary, &[]),
        ];

        for bundle in accepted {
            validate_committed_bundle_shape(&bundle)
                .unwrap_or_else(|err| panic!("valid {:?} shape rejected: {err}", bundle.kind));
        }
    }

    #[test]
    fn shared_bundle_validator_rejects_ambiguous_or_non_dense_shapes() {
        let mut non_dense = structural_bundle(
            FileRowsKind::CheckpointSidecars,
            &[TapeFileKind::ParitySidecar, TapeFileKind::ParitySidecar],
        );
        non_dense.entries[1].tape_file_number = 9;
        let mut multi_block_bootstrap =
            structural_bundle(FileRowsKind::BotBootstrap, &[TapeFileKind::Bootstrap]);
        multi_block_bootstrap.entries[0].block_count = 2;
        let mut non_bot_bootstrap =
            structural_bundle(FileRowsKind::BotBootstrap, &[TapeFileKind::Bootstrap]);
        non_bot_bootstrap.entries[0].tape_file_number = 1;
        let mut empty_terminal = structural_bundle(
            FileRowsKind::TerminalComponent,
            &[TapeFileKind::TapeIndexReplica],
        );
        empty_terminal.entries[0].block_count = 0;
        let rejected = [
            structural_bundle(FileRowsKind::BotBootstrap, &[]),
            structural_bundle(FileRowsKind::CheckpointSidecars, &[]),
            structural_bundle(FileRowsKind::ResumeSidecars, &[]),
            structural_bundle(
                FileRowsKind::Object,
                &[TapeFileKind::Bootstrap, TapeFileKind::Object],
            ),
            structural_bundle(
                FileRowsKind::Object,
                &[TapeFileKind::Object, TapeFileKind::ParityMap],
            ),
            structural_bundle(
                FileRowsKind::CheckpointSidecars,
                &[TapeFileKind::ParitySidecar, TapeFileKind::Bootstrap],
            ),
            structural_bundle(
                FileRowsKind::TerminalComponent,
                &[
                    TapeFileKind::TapeIndexReplica,
                    TapeFileKind::IndexSeparationExtent,
                ],
            ),
            structural_bundle(
                FileRowsKind::Object,
                &[TapeFileKind::Object, TapeFileKind::TapeIndexReplica],
            ),
            non_dense,
            multi_block_bootstrap,
            non_bot_bootstrap,
            empty_terminal,
        ];

        for bundle in rejected {
            assert!(
                validate_committed_bundle_shape(&bundle).is_err(),
                "invalid {:?} shape accepted",
                bundle.kind
            );
        }
    }

    #[test]
    fn to_map_entry_drops_hint_and_canonical_digest_is_unchanged() {
        // REM-PARITY §7.1: the span rides the journal record only. The
        // map-entry field set and the seven-element canonical digest array
        // must be byte-identical with the hint populated and absent —
        // otherwise bootstrap digests change, which is an on-tape break.
        let without_hint = sample_bundle();
        let mut with_hint = sample_bundle();
        with_hint.entries[0].physical_start_hint = Some(2);
        with_hint.entries[1].physical_start_hint = Some(9);

        for (hinted, bare) in with_hint.entries.iter().zip(without_hint.entries.iter()) {
            assert_eq!(
                hinted.to_map_entry(),
                bare.to_map_entry(),
                "to_map_entry must keep dropping the hint"
            );
        }

        let map_with_hint = FilemarkMap::new(
            std::iter::once(TapeFileMapEntry::bootstrap(0, 1))
                .chain(with_hint.entries.iter().map(|entry| {
                    let mut entry = entry.to_map_entry();
                    entry.tape_file_number += 1;
                    entry
                }))
                .collect(),
        )
        .expect("hinted entries build a valid map");
        let map_without_hint = FilemarkMap::new(
            std::iter::once(TapeFileMapEntry::bootstrap(0, 1))
                .chain(without_hint.entries.iter().map(|entry| {
                    let mut entry = entry.to_map_entry();
                    entry.tape_file_number += 1;
                    entry
                }))
                .collect(),
        )
        .expect("bare entries build a valid map");
        assert_eq!(
            map_with_hint
                .canonical_projection_bytes()
                .expect("hinted canonical projection encodes"),
            map_without_hint
                .canonical_projection_bytes()
                .expect("bare canonical projection encodes"),
            "the §7.1 seven-element canonical projection must be byte-identical"
        );
        assert_eq!(
            map_with_hint.canonical_digest().expect("hinted digest"),
            map_without_hint.canonical_digest().expect("bare digest"),
            "bootstrap digests must not move when the hint is populated"
        );
    }

    fn small_scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("journal-test"),
            data_blocks_per_stripe: 3,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 2,
        }
    }

    #[test]
    fn committed_state_validates_restart_bound() {
        let scheme = small_scheme();
        let ok = FileRowsState {
            entries: Vec::new(),
            highest_protected_ordinal: 6,
            total_committed_ordinals: 11,
            orphaned_bundles: Vec::new(),
        };
        ok.validate_v1_restart_bound(&scheme)
            .expect("less than one open epoch is resumable");

        let full_epoch = FileRowsState {
            entries: Vec::new(),
            highest_protected_ordinal: 6,
            total_committed_ordinals: 12,
            orphaned_bundles: Vec::new(),
        };
        let err = full_epoch
            .validate_v1_restart_bound(&scheme)
            .expect_err("one full open epoch is a corrupt or legacy journal");
        assert!(matches!(err, ParityError::ResumeAppend(_)));

        let incoherent = FileRowsState {
            entries: Vec::new(),
            highest_protected_ordinal: 12,
            total_committed_ordinals: 11,
            orphaned_bundles: Vec::new(),
        };
        let err = incoherent
            .validate_v1_restart_bound(&scheme)
            .expect_err("W>T is incoherent");
        assert!(matches!(err, ParityError::ResumeAppend(_)));
    }
}
