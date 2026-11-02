//! Semantic compatibility inputs for fixtures, with no legacy host storage.
use crate::authority::{TerminalFinalizationLayout, TerminalFinalizationProgress};
use crate::{
    NativeObjectCopyProjectionInput, NativeObjectFileProjectionInput, NativeObjectProjectionInput,
    StateError,
};
use remanence_parity::authority::{
    ManualTerminalFinalizationIdentity, TerminalFinalizationTrigger,
};

/// Exact parity close plan persisted before any terminal-prefix media motion.
///
/// Parity-off finalization carries no prefix plan. A parity tape persists this
/// value together with `BeforeReplicaA`, making the final sidecar/ParityMap
/// prefix reconstructible without consulting mutable sink state after restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalFinalizationPrefixPlan {
    /// Immutable planning software and time for the ParityMap and edition.
    pub diagnostics: remanence_parity::ParityMapDiagnostics,
    /// First tape-file number the prefix may emit.
    pub start_tape_file_number: u64,
    /// First tape-file number reserved for replica A.
    pub tail_start_tape_file_number: u64,
    /// Physical cursor before prefix emission.
    pub start_lba: u64,
    /// Exact physical cursor where replica A begins.
    pub tail_start_lba: u64,
    /// Final external ParityMap tape file, when required.
    pub parity_map_tape_file_number: Option<u64>,
    /// Exact post-prefix sidecar directory used to build the final ParityMap.
    pub sidecar_directory_entries: Vec<TerminalFinalizationSidecarDirectoryEntry>,
    /// Exact TerminalPrefix rows and post-prefix W/T authority.
    pub committed_bundle: remanence_parity::FileRowsReport,
}

/// Fixture input for one final sidecar-directory row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalFinalizationSidecarDirectoryEntry {
    /// Sidecar tape-file number.
    pub tape_file_number: u64,
    /// Parity epoch identifier.
    pub epoch_id: u64,
    /// First protected data ordinal.
    pub protected_ordinal_start: u64,
    /// End-exclusive protected data ordinal.
    pub protected_ordinal_end_exclusive: u64,
    /// Total records in the sidecar tape file.
    pub sidecar_total_block_count: u64,
    /// Records in one sidecar header/index copy.
    pub sidecar_header_block_count: u64,
    /// Raw parity-shard record count.
    pub parity_shard_block_count: u64,
    /// Canonical sidecar metadata digest.
    pub canonical_metadata_hash: [u8; 32],
    /// Addendum-defined structural flags.
    pub flags: u32,
}

impl From<&remanence_parity::SidecarEpochDirectoryEntry>
    for TerminalFinalizationSidecarDirectoryEntry
{
    fn from(entry: &remanence_parity::SidecarEpochDirectoryEntry) -> Self {
        Self {
            tape_file_number: entry.tape_file_number,
            epoch_id: entry.epoch_id,
            protected_ordinal_start: entry.protected_ordinal_start,
            protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
            sidecar_total_block_count: entry.sidecar_total_block_count,
            sidecar_header_block_count: entry.sidecar_header_block_count,
            parity_shard_block_count: entry.parity_shard_block_count,
            canonical_metadata_hash: entry.canonical_metadata_hash,
            flags: entry.flags,
        }
    }
}

impl From<&TerminalFinalizationSidecarDirectoryEntry>
    for remanence_parity::SidecarEpochDirectoryEntry
{
    fn from(entry: &TerminalFinalizationSidecarDirectoryEntry) -> Self {
        Self {
            tape_file_number: entry.tape_file_number,
            epoch_id: entry.epoch_id,
            protected_ordinal_start: entry.protected_ordinal_start,
            protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
            sidecar_total_block_count: entry.sidecar_total_block_count,
            sidecar_header_block_count: entry.sidecar_header_block_count,
            parity_shard_block_count: entry.parity_shard_block_count,
            canonical_metadata_hash: entry.canonical_metadata_hash,
            flags: entry.flags,
        }
    }
}

impl TerminalFinalizationPrefixPlan {
    fn validate(&self) -> Result<(), StateError> {
        crate::authority::AuthorityPrefixPlan::from(self).validate()
    }
}

impl From<&remanence_parity::TerminalPrefixPlan> for TerminalFinalizationPrefixPlan {
    fn from(plan: &remanence_parity::TerminalPrefixPlan) -> Self {
        Self {
            diagnostics: plan.diagnostics.clone(),
            start_tape_file_number: plan.start_tape_file_number,
            tail_start_tape_file_number: plan.tail_start_tape_file_number,
            start_lba: plan.start_lba,
            tail_start_lba: plan.tail_start_lba,
            parity_map_tape_file_number: plan.parity_map_tape_file_number,
            sidecar_directory_entries: plan
                .sidecar_directory_entries
                .iter()
                .map(TerminalFinalizationSidecarDirectoryEntry::from)
                .collect(),
            committed_bundle: plan.committed_bundle.clone(),
        }
    }
}

impl TryFrom<&TerminalFinalizationPrefixPlan> for remanence_parity::TerminalPrefixPlan {
    type Error = StateError;

    fn try_from(plan: &TerminalFinalizationPrefixPlan) -> Result<Self, Self::Error> {
        plan.validate()?;
        Ok(Self {
            diagnostics: plan.diagnostics.clone(),
            start_tape_file_number: plan.start_tape_file_number,
            tail_start_tape_file_number: plan.tail_start_tape_file_number,
            start_lba: plan.start_lba,
            tail_start_lba: plan.tail_start_lba,
            parity_map_tape_file_number: plan.parity_map_tape_file_number,
            sidecar_directory_entries: plan
                .sidecar_directory_entries
                .iter()
                .map(remanence_parity::SidecarEpochDirectoryEntry::from)
                .collect(),
            committed_bundle: plan.committed_bundle.clone(),
        })
    }
}

/// Single durable authority for terminal admission, plan, and progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalFinalizationIntent {
    /// Physical tape identity.
    pub tape_uuid: [u8; 16],
    /// Trigger that closed Object admission.
    pub trigger: TerminalFinalizationTrigger,
    /// Present only for a state-changing manual close request.
    pub manual: Option<ManualTerminalFinalizationIdentity>,
    /// Current barrier-proved component boundary.
    pub progress: TerminalFinalizationProgress,
    /// Durable classification that the current component boundary requires
    /// media reconciliation before it may advance again.
    pub recovery_required: bool,
    /// Nonzero final edition identity.
    pub edition_id: [u8; 16],
    /// Nonzero monotonic final edition sequence.
    pub edition_sequence: u64,
    /// Canonical final-scope payload/edition digest.
    pub edition_digest: [u8; 32],
    /// Bounded printable writer identity included in the edition digest.
    pub writer_version: String,
    /// Bounded RFC3339 write time included in the edition digest.
    pub write_timestamp: String,
    /// Exact parity close plan before replica A; absent only for parity-off.
    pub terminal_prefix: Option<TerminalFinalizationPrefixPlan>,
    /// Immutable five-component physical plan.
    pub layout: TerminalFinalizationLayout,
}

impl TerminalFinalizationIntent {
    /// Validate the fixture plan through the native semantic rules.
    pub fn validate_for_tape(&self, tape_uuid: [u8; 16]) -> Result<(), StateError> {
        crate::authority::AuthorityFinalization::from(self).validate_for_tape(tape_uuid)
    }
}

/// Fixture input for one REM-OBJECT terminal recovery row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectRecoveryFixture {
    /// Filemark-delimited tape-file number of the object copy.
    pub tape_file_number: u64,
    /// Number of fixed-size records occupied by the stored copy.
    pub stored_block_count: u64,
    /// Verbatim 1–64-byte REM-OBJECT object identifier.
    pub object_id: Vec<u8>,
    /// Representation-specific recovery anchors.
    pub representation: ObjectRecoveryFixtureRepresentation,
}

/// Stable representation-specific payload for terminal Object recovery.
#[derive(Clone, Debug, PartialEq, Eq)]

pub enum ObjectRecoveryFixtureRepresentation {
    /// Plaintext REM-OBJECT manifest anchors.
    Plaintext {
        /// Object-local body LBA of the manifest payload.
        manifest_first_chunk_lba: u64,
        /// Manifest byte length.
        manifest_size_bytes: u64,
        /// Manifest block count.
        manifest_chunk_count: u64,
        /// SHA-256 digest of the manifest CBOR.
        manifest_sha256: [u8; 32],
    },
    /// Encrypted REM-OBJECT envelope anchors.
    Encrypted {
        /// Recipient epoch identifiers in the key frame.
        recipient_epoch_ids: Vec<[u8; 16]>,
        /// Encrypted metadata frame length.
        metadata_frame_len: u64,
        /// Serialized key-frame length.
        key_frame_len: u32,
    },
}

impl ObjectRecoveryFixture {
    /// Convert the stable checkpoint row into the parity-layer recovery row.
    pub fn to_parity_row(&self) -> remanence_parity::ObjectRecoveryRow {
        let row = match &self.representation {
            ObjectRecoveryFixtureRepresentation::Plaintext {
                manifest_first_chunk_lba,
                manifest_size_bytes,
                manifest_chunk_count,
                manifest_sha256,
            } => remanence_parity::ObjectRecoveryRow::plaintext(
                self.tape_file_number,
                self.stored_block_count,
                *manifest_first_chunk_lba,
                *manifest_size_bytes,
                *manifest_chunk_count,
                *manifest_sha256,
            ),
            ObjectRecoveryFixtureRepresentation::Encrypted {
                recipient_epoch_ids,
                metadata_frame_len,
                key_frame_len,
            } => remanence_parity::ObjectRecoveryRow::encrypted(
                self.tape_file_number,
                self.stored_block_count,
                recipient_epoch_ids.clone(),
                *metadata_frame_len,
                *key_frame_len,
            ),
        };
        row.with_object_id(self.object_id.clone())
    }
}

/// Replayable SQLite projection for one parity-off object in a checkpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectProjectionFixture {
    /// Catalog object row.
    pub object: NativeObjectProjectionInput,
    /// Catalog member-file rows.
    pub files: Vec<NativeObjectFileProjectionInput>,
    /// The single committed copy on this tape.
    pub copy: NativeObjectCopyProjectionInput,
    /// Fixed tape block size.
    pub block_size: u32,
    /// Stored object block count before its delimiter.
    pub block_count: u64,
    /// Whether this object's bundle also projects the BOT bootstrap tape file.
    pub fresh_tape: bool,
    /// Cumulative committed object-data ordinals after this object.
    pub total_committed_ordinals: u64,
    /// REM-OBJECT recovery authority retained for the terminal tape index.
    pub object_recovery_row: ObjectRecoveryFixture,
}

/// Test input converted to a native commit; deliberately has no host codec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitFixture {
    /// Monotonic checkpoint ordinal, starting at one.
    pub ordinal: u64,
    /// Cumulative committed object count after this checkpoint.
    pub committed_object_count: u64,
    /// Barrier-proved EOD partition.
    pub eod_partition: u32,
    /// Barrier-proved EOD logical block address.
    pub eod_lba: u64,
    /// Physical tape UUID, independent of library identity.
    pub tape_uuid: [u8; 16],
    /// Session batch identifier associated with this authority transition.
    pub batch_id: [u8; 16],
    /// Next dense tape-file number available after this barrier.
    ///
    /// Ordinary checkpoints do not emit an authority tape file. Structured
    /// terminal completion therefore names one past final replica C.
    pub next_tape_file_number: u64,
    /// Fixed tape block size used by this tape.
    pub block_size: u32,
    /// Replayable object projections made durable by an ordinary checkpoint.
    /// Terminal-seal records carry no objects.
    pub objects: Vec<ObjectProjectionFixture>,
    /// Parity scheme for parity-protected checkpoint batches. `None` denotes
    /// the historical parity-off record shape.
    pub scheme: Option<remanence_parity::ParityScheme>,
    /// Per-object Layer 3c bundles, in the same order as `objects`.
    pub object_tape_file_bundles: Vec<remanence_parity::FileRowsReport>,
    /// Optional sidecar/ParityMap structural bundle emitted by a parity
    /// barrier, or structured final replica C at terminal completion.
    pub barrier_bundle: Option<remanence_parity::FileRowsReport>,
    /// Completed native terminal finalization in a seal fixture.
    pub terminal_finalization: Option<TerminalFinalizationIntent>,
    /// Whether the fixture supplies terminal completion after all components.
    pub sealed_after_write: bool,
    /// Deferred terminal finalization, durable in the checkpoint's own fsync.
    pub seal_due: Option<TerminalFinalizationTrigger>,
}

/// Ordinary checkpoint prefix and pending structured intent loaded under the
/// exclusive terminal-recovery lease without admitting Object append.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalRecoveryFixture {
    /// CRC-validated checkpoint authority, including a completion if present.
    pub records: Vec<CommitFixture>,
    /// Pending structured terminal-finalization intent.
    pub finalization_intent: Option<TerminalFinalizationIntent>,
}

#[cfg(any(test, feature = "fixtures"))]
impl From<&CommitFixture> for super::AuthorityCommit {
    fn from(record: &CommitFixture) -> Self {
        let last_files = record
            .barrier_bundle
            .as_ref()
            .or_else(|| record.object_tape_file_bundles.last());
        let protected = last_files.map_or(0, |unit| unit.highest_protected_ordinal);
        let total = last_files.map_or_else(
            || {
                record
                    .objects
                    .last()
                    .map_or(0, |object| object.total_committed_ordinals)
            },
            |unit| unit.total_committed_ordinals,
        );
        Self {
            session_id: Some([9; 16]),
            committed_at_utc: "2026-11-02T00:00:00Z".into(),
            protected,
            total,
            ordinal: record.ordinal,
            object_count: record.committed_object_count,
            cursor: remanence_parity::CommitCursor {
                partition: record.eod_partition,
                lba: record.eod_lba,
            },
            tape_uuid: record.tape_uuid,
            operation_id: record.batch_id,
            next_file: record.next_tape_file_number,
            block_size: record.block_size,
            payloads: record
                .objects
                .iter()
                .map(|object| super::ObjectPayload {
                    projection: object.into(),
                    files: object.files.clone(),
                })
                .collect(),
            scheme: record.scheme.clone(),
            object_files: record
                .object_tape_file_bundles
                .iter()
                .map(|b| {
                    remanence_parity::authority::FilePublication::try_from(b)
                        .expect("fixture object publication")
                })
                .collect(),
            barrier: record.barrier_bundle.as_ref().map(|b| {
                remanence_parity::authority::FilePublication::try_from(b)
                    .expect("fixture barrier publication")
            }),
            finalization: record.terminal_finalization.as_ref().map(Into::into),
            sealed: record.sealed_after_write,
            seal_due: record.seal_due,
            check: None,
        }
    }
}
impl From<&super::AuthorityCommit> for CommitFixture {
    fn from(commit: &super::AuthorityCommit) -> Self {
        Self {
            ordinal: commit.ordinal,
            committed_object_count: commit.object_count,
            eod_partition: commit.cursor.partition,
            eod_lba: commit.cursor.lba,
            tape_uuid: commit.tape_uuid,
            batch_id: commit.operation_id,
            next_tape_file_number: commit.next_file,
            block_size: commit.block_size,
            objects: commit
                .payloads
                .iter()
                .map(|payload| crate::ObjectProjectionFixture {
                    object: payload.projection.object.clone(),
                    files: payload.files.clone(),
                    copy: payload.projection.copy.clone(),
                    block_size: payload.projection.block_size,
                    block_count: payload.projection.block_count,
                    fresh_tape: payload.projection.fresh_tape,
                    total_committed_ordinals: payload.projection.total_committed_ordinals,
                    object_recovery_row: fixture_recovery_row(
                        &payload.projection.object_recovery_row,
                    ),
                })
                .collect(),
            scheme: commit.scheme.clone(),
            object_tape_file_bundles: commit.object_files.iter().map(Into::into).collect(),
            barrier_bundle: commit.barrier.as_ref().map(Into::into),
            terminal_finalization: commit.finalization.as_ref().map(Into::into),
            sealed_after_write: commit.sealed,
            seal_due: commit.seal_due,
        }
    }
}

fn fixture_recovery_row(row: &remanence_parity::ObjectRecoveryRow) -> crate::ObjectRecoveryFixture {
    use crate::ObjectRecoveryFixtureRepresentation as Old;
    use remanence_parity::ObjectRecoveryRepresentation as Neutral;
    crate::ObjectRecoveryFixture {
        tape_file_number: row.tape_file_number,
        stored_block_count: row.stored_block_count,
        object_id: row.object_id.clone().unwrap_or_default(),
        representation: match &row.representation {
            Neutral::Plaintext {
                manifest_first_chunk_lba,
                manifest_size_bytes,
                manifest_chunk_count,
                manifest_sha256,
            } => Old::Plaintext {
                manifest_first_chunk_lba: *manifest_first_chunk_lba,
                manifest_size_bytes: *manifest_size_bytes,
                manifest_chunk_count: *manifest_chunk_count,
                manifest_sha256: *manifest_sha256,
            },
            Neutral::Encrypted {
                recipient_epoch_ids,
                metadata_frame_len,
                key_frame_len,
            } => Old::Encrypted {
                recipient_epoch_ids: recipient_epoch_ids.clone(),
                metadata_frame_len: *metadata_frame_len,
                key_frame_len: *key_frame_len,
            },
        },
    }
}

impl From<CommitFixture> for super::AuthorityCommit {
    fn from(record: CommitFixture) -> Self {
        Self::from(&record)
    }
}

impl From<&TerminalFinalizationIntent> for super::AuthorityFinalization {
    fn from(value: &TerminalFinalizationIntent) -> Self {
        Self {
            tape_uuid: value.tape_uuid,
            trigger: value.trigger,
            manual: value.manual.clone(),
            progress: value.progress,
            recovery_required: value.recovery_required,
            edition_id: value.edition_id,
            edition_sequence: value.edition_sequence,
            edition_digest: value.edition_digest,
            writer_version: value.writer_version.clone(),
            write_timestamp: value.write_timestamp.clone(),
            layout: value.layout.clone(),
            terminal_prefix: value.terminal_prefix.as_ref().map(Into::into),
        }
    }
}

impl From<&super::AuthorityFinalization> for TerminalFinalizationIntent {
    fn from(value: &super::AuthorityFinalization) -> Self {
        Self {
            tape_uuid: value.tape_uuid,
            trigger: value.trigger,
            manual: value.manual.clone(),
            progress: value.progress,
            recovery_required: value.recovery_required,
            edition_id: value.edition_id,
            edition_sequence: value.edition_sequence,
            edition_digest: value.edition_digest,
            writer_version: value.writer_version.clone(),
            write_timestamp: value.write_timestamp.clone(),
            layout: value.layout.clone(),
            terminal_prefix: value.terminal_prefix.as_ref().map(Into::into),
        }
    }
}

impl From<&crate::TerminalFinalizationPrefixPlan> for super::AuthorityPrefixPlan {
    fn from(value: &crate::TerminalFinalizationPrefixPlan) -> Self {
        Self {
            diagnostics: value.diagnostics.clone(),
            start_tape_file_number: value.start_tape_file_number,
            tail_start_tape_file_number: value.tail_start_tape_file_number,
            start_lba: value.start_lba,
            tail_start_lba: value.tail_start_lba,
            parity_map_tape_file_number: value.parity_map_tape_file_number,
            sidecar_directory_entries: value
                .sidecar_directory_entries
                .iter()
                .map(Into::into)
                .collect(),
            publication: remanence_parity::authority::FilePublication::try_from(
                &value.committed_bundle,
            )
            .expect("fixture terminal prefix"),
        }
    }
}

impl From<&super::AuthorityPrefixPlan> for crate::TerminalFinalizationPrefixPlan {
    fn from(value: &super::AuthorityPrefixPlan) -> Self {
        Self {
            diagnostics: value.diagnostics.clone(),
            start_tape_file_number: value.start_tape_file_number,
            tail_start_tape_file_number: value.tail_start_tape_file_number,
            start_lba: value.start_lba,
            tail_start_lba: value.tail_start_lba,
            parity_map_tape_file_number: value.parity_map_tape_file_number,
            sidecar_directory_entries: value
                .sidecar_directory_entries
                .iter()
                .map(Into::into)
                .collect(),
            committed_bundle: (&value.publication).into(),
        }
    }
}

impl From<&crate::ObjectProjectionFixture> for crate::tape_log::payload::CommittedObjectProjection {
    fn from(v: &crate::ObjectProjectionFixture) -> Self {
        Self {
            object: v.object.clone(),
            copy: v.copy.clone(),
            block_size: v.block_size,
            block_count: v.block_count,
            fresh_tape: v.fresh_tape,
            total_committed_ordinals: v.total_committed_ordinals,
            object_recovery_row: v.object_recovery_row.to_parity_row(),
        }
    }
}

#[cfg(any(test, feature = "fixtures"))]
impl crate::CatalogIndex {
    /// Adapter for isolated semantic projection fixtures.
    pub fn index_committed_tape_journal(
        &mut self,
        input: crate::TapeJournalIndexInput,
        state: &remanence_parity::FileRowsState,
    ) -> Result<crate::TapeJournalIndexReport, StateError> {
        self.index_structural_authority(input, &state.into())
    }
    /// Adapter for isolated semantic projection fixtures.
    pub fn project_committed_tape_file_bundle(
        &mut self,
        input: crate::TapeJournalIndexInput,
        bundle: &remanence_parity::FileRowsReport,
    ) -> Result<crate::TapeJournalIndexReport, StateError> {
        self.project_file_publication(
            input,
            &remanence_parity::authority::FilePublication::try_from(bundle)
                .map_err(|e| StateError::JournalReplayFailed(e.to_string()))?,
        )
    }
}

impl From<&crate::ObjectProjectionFixture> for super::ObjectPayload {
    fn from(object: &crate::ObjectProjectionFixture) -> Self {
        Self {
            projection: object.into(),
            files: object.files.clone(),
        }
    }
}
