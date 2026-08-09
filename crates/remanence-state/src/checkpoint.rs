//! Durable parity-off checkpoint journal and replayable batch projections.
//!
//! The append-only journal is the numbering and recovery-position authority.
//! A versioned, tape-bound header precedes length-framed JSON records whose
//! CRC-64 covers their version, length, and payload. Each record is fsynced
//! before its corresponding SQLite batch projection. Replay stops at a torn
//! final frame and fails closed on corrupt or unsupported bytes.
//! All production write admission must acquire this authority through
//! [`FileCheckpointJournal::acquire_exclusive`] or, for an already-Finalizing
//! tape, [`FileCheckpointJournal::acquire_exclusive_for_terminal_recovery`].
//! Opening the backing file directly is not a valid write-admission path and
//! would bypass the durable Finalizing fence.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::{
    NativeObjectCopyProjectionInput, NativeObjectFileProjectionInput, NativeObjectProjectionInput,
    StateError,
};

const CHECKPOINT_JOURNAL_SUFFIX: &str = ".remcheckpoint";
const CHECKPOINT_JOURNAL_MAGIC: &[u8; 8] = b"REMCKPT\x01";
const CHECKPOINT_FINALIZATION_INTENT_MAGIC: &[u8; 8] = b"REMFINT\x01";
const CHECKPOINT_JOURNAL_HEADER_LEN: u64 = 8 + 16 + 8;
const CHECKPOINT_RECORD_VERSION: u16 = 2;
const CHECKPOINT_FINALIZATION_INTENT_VERSION: u16 = 2;
const CHECKPOINT_RECORD_PREFIX_LEN: u64 = 2 + 4;
const MAX_CHECKPOINT_RECORD_LEN: u64 = 64 * 1024 * 1024;
const MAX_FINALIZATION_INTENT_LEN: u64 = MAX_CHECKPOINT_RECORD_LEN;

/// Durable trigger that permanently closes Object admission on a tape.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFinalizationTrigger {
    /// A committed Object reached the configured low watermark.
    ReachedLowWatermark,
    /// The drive reported early warning at a committed boundary.
    HardwareEarlyWarning,
    /// An authenticated operator requested early close-out.
    OperatorCloseOut,
    /// An authenticated operator requested pool-wide close-out.
    PoolCloseOut,
    /// No queued Object can fit while preserving the terminal reserve.
    NoPendingObjectFits,
}

/// Authoritative progress through the five terminal tape files.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFinalizationProgress {
    /// No terminal component has been barrier-proved yet.
    BeforeReplicaA,
    /// Replica A and its trailing filemark are barrier-proved.
    AfterReplicaA,
    /// Separation AB and its trailing filemark are barrier-proved.
    AfterSeparationAb,
    /// Replica B and its trailing filemark are barrier-proved.
    AfterReplicaB,
    /// Separation BC and its trailing filemark are barrier-proved.
    AfterSeparationBc,
    /// Replica C and its trailing filemark are barrier-proved.
    AfterReplicaC,
}

impl TerminalFinalizationProgress {
    /// The only legal next durable state.
    pub const fn successor(self) -> Option<Self> {
        match self {
            Self::BeforeReplicaA => Some(Self::AfterReplicaA),
            Self::AfterReplicaA => Some(Self::AfterSeparationAb),
            Self::AfterSeparationAb => Some(Self::AfterReplicaB),
            Self::AfterReplicaB => Some(Self::AfterSeparationBc),
            Self::AfterSeparationBc => Some(Self::AfterReplicaC),
            Self::AfterReplicaC => None,
        }
    }

    /// Number of complete replica files proved by this state.
    pub const fn completed_replicas(self) -> u8 {
        match self {
            Self::BeforeReplicaA => 0,
            Self::AfterReplicaA | Self::AfterSeparationAb => 1,
            Self::AfterReplicaB | Self::AfterSeparationBc => 2,
            Self::AfterReplicaC => 3,
        }
    }
}

/// Structural kind persisted in a terminal-finalization plan.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFinalizationComponentKind {
    /// Complete final-scope tape-index replica.
    TapeIndexReplica,
    /// Typed fixed-record physical separation extent.
    IndexSeparationExtent,
}

/// One planned filemark-delimited terminal component.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TerminalFinalizationComponent {
    /// Structural kind.
    pub kind: TerminalFinalizationComponentKind,
    /// One-based ordinal within the kind.
    pub ordinal: u16,
    /// Dense tape-file number from BOT.
    pub tape_file_number: u64,
    /// Planned component start LBA.
    pub start_lba: u64,
    /// Fixed records before the trailing filemark.
    pub record_count: u64,
}

/// Complete immutable A/gap/B/gap/C plan captured before tape motion.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TerminalFinalizationLayout {
    /// SCSI partition; the draft profile permits partition zero only.
    pub partition: u32,
    /// Fixed record size shared by all five files.
    pub block_size: u32,
    /// A, gap AB, B, gap BC, C.
    pub components: [TerminalFinalizationComponent; 5],
    /// Planned EOD after C's trailing filemark.
    pub expected_eod_lba: u64,
    /// Digest of the canonical planned layout.
    pub layout_digest: [u8; 32],
}

impl TerminalFinalizationLayout {
    fn to_parity_layout(&self) -> remanence_parity::TerminalTailLayout {
        let components =
            self.components
                .map(|component| remanence_parity::TerminalTailComponentPlan {
                    kind: match component.kind {
                        TerminalFinalizationComponentKind::TapeIndexReplica => {
                            remanence_parity::TerminalTailComponentKind::TapeIndexReplica
                        }
                        TerminalFinalizationComponentKind::IndexSeparationExtent => {
                            remanence_parity::TerminalTailComponentKind::IndexSeparationExtent
                        }
                    },
                    ordinal: component.ordinal,
                    planned_tape_file_number: component.tape_file_number,
                    planned_start_lba: component.start_lba,
                    record_count: component.record_count,
                });
        remanence_parity::TerminalTailLayout {
            partition: self.partition,
            block_size: self.block_size,
            components,
            expected_eod_lba: self.expected_eod_lba,
        }
    }

    fn validate(&self) -> Result<(), StateError> {
        let layout = self.to_parity_layout();
        layout.validate().map_err(|error| {
            StateError::JournalReplayFailed(format!(
                "terminal finalization layout is invalid: {error}"
            ))
        })?;
        let digest = layout.digest().map_err(|error| {
            StateError::JournalReplayFailed(format!(
                "terminal finalization layout digest failed: {error}"
            ))
        })?;
        if digest != self.layout_digest {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization layout digest mismatch".to_string(),
            ));
        }
        Ok(())
    }

    /// Reconstruct the checked parity-layer layout used by terminal codecs.
    pub fn try_to_parity_layout(&self) -> Result<remanence_parity::TerminalTailLayout, StateError> {
        self.validate()?;
        Ok(self.to_parity_layout())
    }
}

impl TryFrom<remanence_parity::TerminalTailLayout> for TerminalFinalizationLayout {
    type Error = StateError;

    fn try_from(layout: remanence_parity::TerminalTailLayout) -> Result<Self, Self::Error> {
        layout.validate().map_err(|error| {
            StateError::JournalReplayFailed(format!(
                "terminal finalization layout is invalid: {error}"
            ))
        })?;
        let layout_digest = layout.digest().map_err(|error| {
            StateError::JournalReplayFailed(format!(
                "terminal finalization layout digest failed: {error}"
            ))
        })?;
        let components = layout
            .components
            .map(|component| TerminalFinalizationComponent {
                kind: match component.kind {
                    remanence_parity::TerminalTailComponentKind::TapeIndexReplica => {
                        TerminalFinalizationComponentKind::TapeIndexReplica
                    }
                    remanence_parity::TerminalTailComponentKind::IndexSeparationExtent => {
                        TerminalFinalizationComponentKind::IndexSeparationExtent
                    }
                },
                ordinal: component.ordinal,
                tape_file_number: component.planned_tape_file_number,
                start_lba: component.planned_start_lba,
                record_count: component.record_count,
            });
        Ok(Self {
            partition: layout.partition,
            block_size: layout.block_size,
            components,
            expected_eod_lba: layout.expected_eod_lba,
            layout_digest,
        })
    }
}

/// Exact parity close plan persisted before any terminal-prefix media motion.
///
/// Parity-off finalization carries no prefix plan. A parity tape persists this
/// value together with `BeforeReplicaA`, making the final sidecar/ParityMap
/// prefix reconstructible without consulting mutable sink state after restart.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TerminalFinalizationPrefixPlan {
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
    pub committed_bundle: remanence_parity::CommittedBundle,
}

/// Stable state-journal representation of one final sidecar-directory row.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
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
        remanence_parity::validate_committed_bundle_shape(&self.committed_bundle).map_err(
            |error| {
                StateError::JournalReplayFailed(format!(
                    "terminal finalization prefix bundle is invalid: {error}"
                ))
            },
        )?;
        if self.committed_bundle.kind != remanence_parity::CommittedBundleKind::TerminalPrefix {
            return Err(StateError::JournalReplayFailed(format!(
                "terminal finalization prefix uses {:?} bundle kind instead of TerminalPrefix",
                self.committed_bundle.kind
            )));
        }
        if self.committed_bundle.highest_protected_ordinal
            > self.committed_bundle.total_committed_ordinals
        {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization prefix W exceeds T".to_string(),
            ));
        }

        let mut expected_file = self.start_tape_file_number;
        let mut expected_lba = self.start_lba;
        let mut observed_parity_map = None;
        for entry in &self.committed_bundle.entries {
            if entry.tape_file_number != expected_file
                || entry.physical_start_hint != Some(expected_lba)
                || entry.block_count == 0
            {
                return Err(StateError::JournalReplayFailed(format!(
                    "terminal finalization prefix entry at tape file {} does not match planned dense file/LBA geometry {expected_file}/{expected_lba}",
                    entry.tape_file_number
                )));
            }
            if entry.object_id.is_some()
                || entry.first_parity_data_ordinal.is_some()
                || entry.object_recovery_row.is_some()
                || entry.canonical_metadata_hash.is_none()
            {
                return Err(StateError::JournalReplayFailed(format!(
                    "terminal finalization prefix entry at tape file {} has invalid common metadata",
                    entry.tape_file_number
                )));
            }
            match entry.kind {
                remanence_parity::TapeFileKind::ParitySidecar => {
                    let (Some(_epoch_id), Some(start), Some(end)) = (
                        entry.epoch_id,
                        entry.protected_ordinal_start,
                        entry.protected_ordinal_end_exclusive,
                    ) else {
                        return Err(StateError::JournalReplayFailed(format!(
                            "terminal finalization sidecar at tape file {} is missing its protected range",
                            entry.tape_file_number
                        )));
                    };
                    if start >= end || end > self.committed_bundle.highest_protected_ordinal {
                        return Err(StateError::JournalReplayFailed(format!(
                            "terminal finalization sidecar at tape file {} has invalid protected range [{start}, {end})",
                            entry.tape_file_number
                        )));
                    }
                }
                remanence_parity::TapeFileKind::ParityMap => {
                    if entry.epoch_id.is_some()
                        || entry.protected_ordinal_start.is_some()
                        || entry.protected_ordinal_end_exclusive.is_some()
                    {
                        return Err(StateError::JournalReplayFailed(format!(
                            "terminal finalization ParityMap at tape file {} carries sidecar fields",
                            entry.tape_file_number
                        )));
                    }
                    observed_parity_map = Some(entry.tape_file_number);
                }
                other => {
                    return Err(StateError::JournalReplayFailed(format!(
                        "terminal finalization prefix contains forbidden {other:?} at tape file {}",
                        entry.tape_file_number
                    )));
                }
            }
            expected_file = expected_file.checked_add(1).ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "terminal finalization prefix tape-file number overflows u64".to_string(),
                )
            })?;
            expected_lba = expected_lba
                .checked_add(entry.block_count)
                .and_then(|lba| lba.checked_add(1))
                .ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "terminal finalization prefix LBA overflows u64".to_string(),
                    )
                })?;
        }
        if expected_file != self.tail_start_tape_file_number
            || expected_lba != self.tail_start_lba
            || observed_parity_map != self.parity_map_tape_file_number
        {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization prefix tail coordinates or ParityMap identity mismatch"
                    .to_string(),
            ));
        }
        if self.sidecar_directory_entries.is_empty() != self.parity_map_tape_file_number.is_none() {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization prefix ParityMap presence does not match its sidecar directory"
                    .to_string(),
            ));
        }
        if let Some(parity_map_tape_file_number) = self.parity_map_tape_file_number {
            let directory = remanence_parity::SidecarEpochDirectory {
                directory_scope_tape_file_count: parity_map_tape_file_number
                    .checked_add(1)
                    .ok_or_else(|| {
                        StateError::JournalReplayFailed(
                            "terminal finalization sidecar directory scope overflows u64"
                                .to_string(),
                        )
                    })?,
                directory_scope_total_data_ordinals: self.committed_bundle.total_committed_ordinals,
                directory_scope_highest_protected_ordinal: self
                    .committed_bundle
                    .highest_protected_ordinal,
                is_final_directory: true,
                entries: self
                    .sidecar_directory_entries
                    .iter()
                    .map(remanence_parity::SidecarEpochDirectoryEntry::from)
                    .collect(),
            };
            directory.validate().map_err(|error| {
                StateError::JournalReplayFailed(format!(
                    "terminal finalization sidecar directory is invalid: {error}"
                ))
            })?;
        }
        Ok(())
    }
}

impl From<&remanence_parity::TerminalPrefixPlan> for TerminalFinalizationPrefixPlan {
    fn from(plan: &remanence_parity::TerminalPrefixPlan) -> Self {
        Self {
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

/// Durable operation and request identity for manual early finalization.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ManualTerminalFinalizationIdentity {
    /// Durable operation UUID.
    pub operation_id: [u8; 16],
    /// Stable Layer-5 operation kind in the idempotency scope.
    pub operation_kind: String,
    /// Authenticated actor fingerprint that scopes the idempotency key.
    pub actor_fingerprint: String,
    /// Caller-provided durable idempotency UUID.
    pub idempotency_key: [u8; 16],
    /// Hash of the exact state-changing request.
    pub request_fingerprint: [u8; 32],
    /// Assignment observed under the per-tape owner.
    pub assigned_pool_id: Option<String>,
    /// Wire presence/value supplied by the caller.
    pub expected_pool_id: Option<String>,
    /// Assignment generation observed under the per-tape owner.
    pub assignment_generation: u64,
    /// Exact operator reason bytes represented as UTF-8.
    pub reason: String,
}

/// Single durable authority for terminal admission, plan, and progress.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
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
    #[serde(default)]
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
    pub(crate) fn validate_for_tape(&self, tape_uuid: [u8; 16]) -> Result<(), StateError> {
        if self.tape_uuid != tape_uuid {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization intent tape UUID does not match journal".to_string(),
            ));
        }
        if self.edition_id == [0; 16]
            || self.edition_sequence == 0
            || self.edition_digest == [0; 32]
        {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization intent has a zero edition identity, sequence, or digest"
                    .to_string(),
            ));
        }
        if self.writer_version.len() > 128
            || !self
                .writer_version
                .bytes()
                .all(|byte| (0x20..=0x7e).contains(&byte))
        {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization writer_version must be at most 128 printable ASCII bytes"
                    .to_string(),
            ));
        }
        if self.write_timestamp.len() > 64
            || !self
                .write_timestamp
                .as_bytes()
                .get(10)
                .is_some_and(|byte| matches!(byte, b'T' | b't'))
            || OffsetDateTime::parse(&self.write_timestamp, &Rfc3339).is_err()
        {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization write_timestamp must be at most 64 bytes of RFC3339"
                    .to_string(),
            ));
        }
        self.layout.validate()?;
        if let Some(prefix) = &self.terminal_prefix {
            prefix.validate()?;
            let replica_a = self.layout.components[0];
            if prefix.tail_start_tape_file_number != replica_a.tape_file_number
                || prefix.tail_start_lba != replica_a.start_lba
            {
                return Err(StateError::JournalReplayFailed(
                    "terminal finalization prefix tail does not match replica A layout".to_string(),
                ));
            }
        }
        match (&self.trigger, &self.manual) {
            (TerminalFinalizationTrigger::OperatorCloseOut, Some(manual)) => {
                if manual.operation_id == [0; 16]
                    || manual.idempotency_key == [0; 16]
                    || manual.request_fingerprint == [0; 32]
                    || manual.operation_kind.is_empty()
                    || manual.actor_fingerprint.is_empty()
                    || manual.reason.is_empty()
                {
                    return Err(StateError::JournalReplayFailed(
                        "manual terminal finalization identity is incomplete".to_string(),
                    ));
                }
                if manual.assigned_pool_id != manual.expected_pool_id {
                    return Err(StateError::JournalReplayFailed(
                        "manual terminal finalization pool guard does not match the owned assignment"
                            .to_string(),
                    ));
                }
            }
            (TerminalFinalizationTrigger::OperatorCloseOut, None) => {
                return Err(StateError::JournalReplayFailed(
                    "operator close-out is missing durable operation identity".to_string(),
                ));
            }
            (_, Some(_)) => {
                return Err(StateError::JournalReplayFailed(
                    "automatic terminal finalization unexpectedly carries manual identity"
                        .to_string(),
                ));
            }
            (_, None) => {}
        }
        Ok(())
    }

    fn validate_for_checkpoint_prefix(
        &self,
        previous: Option<&CheckpointJournalRecord>,
    ) -> Result<(), StateError> {
        self.validate_for_tape(self.tape_uuid)?;
        let previous = previous.ok_or_else(|| {
            StateError::JournalReplayFailed(
                "terminal finalization has no ordinary checkpoint prefix".to_string(),
            )
        })?;
        if previous.sealed_after_write {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization follows sealed checkpoint authority".to_string(),
            ));
        }
        if previous.tape_uuid != self.tape_uuid || previous.block_size != self.layout.block_size {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization prefix tape or block geometry mismatch".to_string(),
            ));
        }
        if previous.scheme.is_some() != self.terminal_prefix.is_some() {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization prefix presence does not match checkpoint parity mode"
                    .to_string(),
            ));
        }
        let expected_start_file = previous.next_tape_file_number;
        let replica_a = self.layout.components[0];
        if let Some(prefix) = &self.terminal_prefix {
            if prefix.start_tape_file_number != expected_start_file
                || prefix.start_lba != previous.eod_lba
            {
                return Err(StateError::JournalReplayFailed(format!(
                    "terminal finalization prefix starts at tape file {}/LBA {}, expected {expected_start_file}/{}",
                    prefix.start_tape_file_number, prefix.start_lba, previous.eod_lba
                )));
            }
            let (prior_highest, prior_total) = checkpoint_record_watermarks(previous);
            if prefix.committed_bundle.total_committed_ordinals != prior_total
                || prefix.committed_bundle.highest_protected_ordinal < prior_highest
            {
                return Err(StateError::JournalReplayFailed(format!(
                    "terminal finalization prefix W/T ({}/{}) does not extend prior authority ({prior_highest}/{prior_total})",
                    prefix.committed_bundle.highest_protected_ordinal,
                    prefix.committed_bundle.total_committed_ordinals
                )));
            }
        } else if replica_a.tape_file_number != expected_start_file
            || replica_a.start_lba != previous.eod_lba
        {
            return Err(StateError::JournalReplayFailed(format!(
                "parity-off terminal layout starts at tape file {}/LBA {}, expected {expected_start_file}/{}",
                replica_a.tape_file_number, replica_a.start_lba, previous.eod_lba
            )));
        }
        Ok(())
    }
}

/// Stable journal representation of one REM-OBJECT terminal recovery row.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CheckpointObjectRecoveryRow {
    /// Filemark-delimited tape-file number of the object copy.
    pub tape_file_number: u64,
    /// Number of fixed-size records occupied by the stored copy.
    pub stored_block_count: u64,
    /// Verbatim 1–64-byte REM-OBJECT object identifier.
    pub object_id: Vec<u8>,
    /// Representation-specific recovery anchors.
    pub representation: CheckpointObjectRecoveryRepresentation,
}

/// Stable representation-specific payload for terminal Object recovery.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CheckpointObjectRecoveryRepresentation {
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

impl CheckpointObjectRecoveryRow {
    /// Convert the stable checkpoint row into the parity-layer recovery row.
    pub fn to_parity_row(&self) -> remanence_parity::ObjectRecoveryRow {
        let row = match &self.representation {
            CheckpointObjectRecoveryRepresentation::Plaintext {
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
            CheckpointObjectRecoveryRepresentation::Encrypted {
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
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CheckpointObjectProjection {
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
    pub object_recovery_row: CheckpointObjectRecoveryRow,
}

/// One fsynced checkpoint or terminal-seal authority record.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CheckpointJournalRecord {
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
    pub objects: Vec<CheckpointObjectProjection>,
    /// Parity scheme for parity-protected checkpoint batches. `None` denotes
    /// the historical parity-off record shape.
    #[serde(default)]
    pub scheme: Option<remanence_parity::ParityScheme>,
    /// Per-object Layer 3c bundles, in the same order as `objects`.
    #[serde(default)]
    pub object_tape_file_bundles: Vec<remanence_parity::CommittedBundle>,
    /// Optional sidecar/ParityMap structural bundle emitted by a parity
    /// barrier, or structured final replica C at terminal completion.
    #[serde(default)]
    pub barrier_bundle: Option<remanence_parity::CommittedBundle>,
    /// Completed structured terminal finalization carried by sealed authority.
    ///
    /// This is absent on ordinary checkpoints. A/gap/B/gap/C finalization records persist the
    /// exact final intent here so replay does not depend on the transient
    /// `.finalizing` companion after that file is durably cleared.
    #[serde(default)]
    pub terminal_finalization: Option<TerminalFinalizationIntent>,
    /// Whether this objectless record proves the tape's terminal boundary.
    ///
    /// The checkpoint journal is the durable Layer 5 authority for replaying
    /// the SQLite `sealed` projection after a crash. A true value requires an
    /// empty `objects` list and an exact structured replica-C completion. The record is appended only
    /// after terminal media and its synchronous barrier have succeeded.
    pub sealed_after_write: bool,
}

/// Checked final-prefix scope/counts streamed into each terminal replica.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalIndexAuthoritySummary {
    /// Exact prefix scope committed before replica A.
    pub scope: remanence_parity::TapeIndexReplicaScope,
    /// Exact structural/Object row counts.
    pub counts: remanence_parity::TapeIndexReplicaCounts,
}

/// Deterministic per-pass replay metrics captured while freezing terminal
/// authority.
///
/// These values describe the single bounded validation scan used to freeze
/// each snapshot. They are not cumulative counters for the additional full
/// scans used to validate source agreement during construction or to emit
/// replica rows later. Peak-live values therefore remain directly comparable
/// between journals regardless of how many replica passes a caller performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalIndexAuthorityReplayMetrics {
    /// Checkpoint-journal validation scan.
    pub checkpoint: CheckpointBoundedReplayMetrics,
    /// Parity-journal validation scan, absent on parity-off tapes.
    pub parity: Option<remanence_parity::BoundedJournalReplayMetrics>,
}

#[derive(Debug)]
enum TerminalIndexAuthorityRows<'a> {
    Parity(&'a remanence_parity::CommittedState),
    NoParity(&'a [CheckpointJournalRecord]),
    ReplayParity {
        checkpoint: CheckpointReplaySnapshot,
        parity: Box<remanence_parity::FileTapeFileJournalCommittedSnapshot>,
        planned_terminal_prefix: Option<Box<TerminalFinalizationPrefixPlan>>,
    },
    ReplayNoParity(CheckpointReplaySnapshot),
}

#[derive(Clone, Copy, Debug)]
enum ReplayTerminalPrefixMode<'a> {
    Absent,
    Planned(&'a TerminalFinalizationPrefixPlan),
    Durable(&'a TerminalFinalizationPrefixPlan),
}

/// Replayable, allocation-bounded source for a final tape-index edition.
///
/// Construction proves a dense structural prefix and an exact one-to-one
/// Object-map/recovery-row relation. Later replica passes stream the same
/// frozen authority and never consult the rebuildable SQLite cache.
#[derive(Debug)]
pub struct CheckpointTerminalIndexRecordSource<'a> {
    rows: TerminalIndexAuthorityRows<'a>,
    summary: TerminalIndexAuthoritySummary,
    tape_uuid: [u8; 16],
}

impl CheckpointTerminalIndexRecordSource<'static> {
    /// Validate retained file-backed checkpoint and parity authorities without
    /// materializing either complete journal projection.
    ///
    /// The returned source owns frozen read handles rather than borrowing the
    /// exclusive authorities. The caller must retain those exclusive handles
    /// while terminal progress is appended; every replica pass replays the
    /// selected CRC-validated, length-bounded prefix.
    pub fn new_replay_backed(
        checkpoint: &FileCheckpointJournalLease,
        parity: &remanence_parity::FileTapeFileJournal,
    ) -> Result<Self, StateError> {
        let checkpoint = checkpoint.bounded_replay_snapshot()?;
        let parity = parity
            .committed_snapshot_bounded()
            .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
        let (summary, tape_uuid) = validate_replay_backed_parity_terminal_rows(
            &checkpoint,
            &parity,
            ReplayTerminalPrefixMode::Absent,
        )?;
        Ok(Self {
            rows: TerminalIndexAuthorityRows::ReplayParity {
                checkpoint,
                parity: Box::new(parity),
                planned_terminal_prefix: None,
            },
            summary,
            tape_uuid,
        })
    }

    /// Validate retained checkpoint authority for a parity-off tape without
    /// materializing its complete checkpoint history. The returned source owns
    /// its frozen read handle and does not borrow the mutable lease.
    pub fn new_replay_backed_no_parity(
        checkpoint: &FileCheckpointJournalLease,
    ) -> Result<Self, StateError> {
        let checkpoint = checkpoint.bounded_replay_snapshot()?;
        let (summary, tape_uuid) = validate_replay_backed_no_parity_terminal_rows(&checkpoint)?;
        Ok(Self {
            rows: TerminalIndexAuthorityRows::ReplayNoParity(checkpoint),
            summary,
            tape_uuid,
        })
    }

    /// Validate retained authorities after the exact planned TerminalPrefix
    /// and its checkpoint marker have become durable. The returned source owns
    /// its frozen read handles and does not borrow either mutable authority.
    pub fn new_replay_backed_after_terminal_prefix(
        checkpoint: &FileCheckpointJournalLease,
        parity: &remanence_parity::FileTapeFileJournal,
        prefix: &TerminalFinalizationPrefixPlan,
    ) -> Result<Self, StateError> {
        let checkpoint = checkpoint.bounded_replay_snapshot()?;
        let parity = parity
            .terminal_prefix_snapshot_bounded()
            .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
        let (summary, tape_uuid) = validate_replay_backed_parity_terminal_rows(
            &checkpoint,
            &parity,
            ReplayTerminalPrefixMode::Durable(prefix),
        )?;
        Ok(Self {
            rows: TerminalIndexAuthorityRows::ReplayParity {
                checkpoint,
                parity: Box::new(parity),
                planned_terminal_prefix: None,
            },
            summary,
            tape_uuid,
        })
    }

    /// Validate the bounded base authority and virtually append an immutable
    /// planned TerminalPrefix before that prefix is written or journaled.
    ///
    /// Edition/layout planning and all later replica passes therefore use the
    /// same final row set, while the caller remains free to advance the
    /// exclusive checkpoint and parity authorities after construction.
    pub fn new_replay_backed_with_planned_terminal_prefix(
        checkpoint: &FileCheckpointJournalLease,
        parity: &remanence_parity::FileTapeFileJournal,
        prefix: &TerminalFinalizationPrefixPlan,
    ) -> Result<Self, StateError> {
        let checkpoint = checkpoint.bounded_replay_snapshot()?;
        let parity = parity
            .planned_terminal_prefix_base_snapshot_bounded(&prefix.committed_bundle)
            .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
        let (summary, tape_uuid) = validate_replay_backed_parity_terminal_rows(
            &checkpoint,
            &parity,
            ReplayTerminalPrefixMode::Planned(prefix),
        )?;
        Ok(Self {
            rows: TerminalIndexAuthorityRows::ReplayParity {
                checkpoint,
                parity: Box::new(parity),
                planned_terminal_prefix: Some(Box::new(prefix.clone())),
            },
            summary,
            tape_uuid,
        })
    }
}

impl<'a> CheckpointTerminalIndexRecordSource<'a> {
    /// Validate one committed checkpoint prefix and optional parity journal.
    pub fn new(
        records: &'a [CheckpointJournalRecord],
        committed: Option<&'a remanence_parity::CommittedState>,
    ) -> Result<Self, StateError> {
        let last = records.last().ok_or_else(|| {
            StateError::JournalReplayFailed(
                "terminal index authority has no committed checkpoint prefix".to_string(),
            )
        })?;
        if last.sealed_after_write {
            return Err(StateError::JournalReplayFailed(
                "terminal index authority already ends in sealed checkpoint state".to_string(),
            ));
        }
        match committed {
            Some(committed) => {
                let scheme = last.scheme.as_ref().ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "parity terminal index authority is missing its scheme".to_string(),
                    )
                })?;
                validate_parity_resume_authority(
                    records,
                    committed,
                    last.tape_uuid,
                    last.block_size,
                    scheme,
                )?;
                let summary = validate_parity_terminal_index_rows(committed)?;
                Ok(Self {
                    rows: TerminalIndexAuthorityRows::Parity(committed),
                    summary,
                    tape_uuid: last.tape_uuid,
                })
            }
            None => {
                let summary = validate_no_parity_terminal_index_rows(records)?;
                Ok(Self {
                    rows: TerminalIndexAuthorityRows::NoParity(records),
                    summary,
                    tape_uuid: last.tape_uuid,
                })
            }
        }
    }

    /// Validate and stream parity authority after the persisted terminal prefix.
    ///
    /// Ordinary resume validation is intentionally applied to `base_committed`
    /// before the planned prefix is considered. `projected_post_prefix` must
    /// then be exactly that base plus the persisted TerminalPrefix bundle;
    /// unrelated journal rows or terminal components fail closed.
    pub fn new_after_terminal_prefix(
        records: &'a [CheckpointJournalRecord],
        base_committed: &remanence_parity::CommittedState,
        projected_post_prefix: &'a remanence_parity::CommittedState,
        prefix: &TerminalFinalizationPrefixPlan,
    ) -> Result<Self, StateError> {
        let last = records.last().ok_or_else(|| {
            StateError::JournalReplayFailed(
                "terminal index authority has no committed checkpoint prefix".to_string(),
            )
        })?;
        if last.sealed_after_write {
            return Err(StateError::JournalReplayFailed(
                "terminal index authority already ends in sealed checkpoint state".to_string(),
            ));
        }
        let scheme = last.scheme.as_ref().ok_or_else(|| {
            StateError::JournalReplayFailed(
                "parity terminal index authority is missing its scheme".to_string(),
            )
        })?;
        validate_parity_resume_authority(
            records,
            base_committed,
            last.tape_uuid,
            last.block_size,
            scheme,
        )?;
        prefix.validate()?;
        let expected_prefix_file = last.next_tape_file_number;
        if prefix.start_tape_file_number != expected_prefix_file || prefix.start_lba != last.eod_lba
        {
            return Err(StateError::JournalReplayFailed(format!(
                "persisted TerminalPrefix starts at tape file {}/LBA {}, expected {expected_prefix_file}/{}",
                prefix.start_tape_file_number, prefix.start_lba, last.eod_lba
            )));
        }
        let base_len = base_committed.entries.len();
        let projected_len = base_len
            .checked_add(prefix.committed_bundle.entries.len())
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "terminal prefix projected entry count overflows usize".to_string(),
                )
            })?;
        if !base_committed.orphaned_bundles.is_empty()
            || !projected_post_prefix.orphaned_bundles.is_empty()
            || projected_post_prefix.entries.len() != projected_len
            || projected_post_prefix.entries.get(..base_len)
                != Some(base_committed.entries.as_slice())
            || projected_post_prefix.entries.get(base_len..)
                != Some(prefix.committed_bundle.entries.as_slice())
            || projected_post_prefix.highest_protected_ordinal
                != prefix.committed_bundle.highest_protected_ordinal
            || projected_post_prefix.total_committed_ordinals
                != prefix.committed_bundle.total_committed_ordinals
        {
            return Err(StateError::JournalReplayFailed(
                "post-prefix parity authority is not exactly the checkpoint base plus persisted TerminalPrefix"
                    .to_string(),
            ));
        }
        let (base_highest, base_total) = (
            base_committed.highest_protected_ordinal,
            base_committed.total_committed_ordinals,
        );
        if prefix.committed_bundle.total_committed_ordinals != base_total
            || prefix.committed_bundle.highest_protected_ordinal < base_highest
        {
            return Err(StateError::JournalReplayFailed(format!(
                "persisted TerminalPrefix W/T ({}/{}) does not extend base parity authority ({base_highest}/{base_total})",
                prefix.committed_bundle.highest_protected_ordinal,
                prefix.committed_bundle.total_committed_ordinals
            )));
        }
        let summary = validate_parity_terminal_index_rows(projected_post_prefix)?;
        Ok(Self {
            rows: TerminalIndexAuthorityRows::Parity(projected_post_prefix),
            summary,
            tape_uuid: last.tape_uuid,
        })
    }

    /// Return the immutable scope/count result established at construction.
    pub const fn summary(&self) -> TerminalIndexAuthoritySummary {
        self.summary
    }

    /// Return the per-pass bounded replay metrics sampled while freezing each
    /// owned journal snapshot. These do not aggregate later construction or
    /// row-emission scans. Legacy slice/flattened-state sources return `None`.
    pub const fn replay_metrics(&self) -> Option<TerminalIndexAuthorityReplayMetrics> {
        match &self.rows {
            TerminalIndexAuthorityRows::ReplayParity {
                checkpoint, parity, ..
            } => Some(TerminalIndexAuthorityReplayMetrics {
                checkpoint: checkpoint.metrics,
                parity: Some(parity.metrics()),
            }),
            TerminalIndexAuthorityRows::ReplayNoParity(checkpoint) => {
                Some(TerminalIndexAuthorityReplayMetrics {
                    checkpoint: checkpoint.metrics,
                    parity: None,
                })
            }
            TerminalIndexAuthorityRows::Parity(_) | TerminalIndexAuthorityRows::NoParity(_) => None,
        }
    }

    /// Reconstruct and verify the exact final edition against streamed rows.
    ///
    /// Scope/counts come from the validated authority source; immutable
    /// identity, diagnostics, layout, and expected digests come from the
    /// persisted intent. A restart or daemon upgrade therefore cannot silently
    /// select a different final edition.
    pub fn reconstruct_final_edition(
        &mut self,
        intent: &TerminalFinalizationIntent,
    ) -> Result<remanence_parity::TapeIndexEditionPlan, StateError> {
        intent.validate_for_tape(self.tape_uuid)?;
        let descriptor = remanence_parity::TapeIndexEditionDescriptor {
            tape_uuid: intent.tape_uuid,
            edition_id: intent.edition_id,
            edition_sequence: intent.edition_sequence,
            scope: self.summary.scope,
            counts: self.summary.counts,
            block_size: intent.layout.block_size,
            compression_enabled: false,
            writer_version: intent.writer_version.clone(),
            write_timestamp: intent.write_timestamp.clone(),
            terminal_layout: intent.layout.try_to_parity_layout()?,
        };
        let plan =
            remanence_parity::plan_tape_index_edition(descriptor, self).map_err(|error| {
                StateError::JournalReplayFailed(format!(
                    "reconstruct terminal final edition from checkpoint authority: {error}"
                ))
            })?;
        if plan.edition_digest != intent.edition_digest
            || plan.layout_digest != intent.layout.layout_digest
        {
            return Err(StateError::JournalReplayFailed(
                "reconstructed terminal edition digest does not match persisted intent".to_string(),
            ));
        }
        Ok(plan)
    }
}

impl remanence_parity::TapeIndexReplicaRecordSource for CheckpointTerminalIndexRecordSource<'_> {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(
            &remanence_parity::TapeIndexReplicaMapEntry,
        ) -> Result<(), remanence_parity::ParityError>,
    ) -> Result<(), remanence_parity::ParityError> {
        match &self.rows {
            TerminalIndexAuthorityRows::Parity(committed) => {
                for entry in &committed.entries {
                    visitor(&terminal_map_entry_from_committed(entry)?)?;
                }
            }
            TerminalIndexAuthorityRows::NoParity(records) => {
                visitor(&remanence_parity::TapeIndexReplicaMapEntry {
                    tape_file_number: 0,
                    kind: remanence_parity::TapeIndexReplicaFileKind::Bootstrap,
                    block_count: 1,
                    first_parity_data_ordinal: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    epoch_id: None,
                })?;
                for record in *records {
                    for object in &record.objects {
                        let first_data_ordinal = object
                            .total_committed_ordinals
                            .checked_sub(object.block_count)
                            .ok_or(remanence_parity::ParityError::Invariant(
                                "validated no-parity Object ordinal range underflowed",
                            ))?;
                        visitor(&remanence_parity::TapeIndexReplicaMapEntry {
                            tape_file_number: object.copy.tape_file_number,
                            kind: remanence_parity::TapeIndexReplicaFileKind::Object,
                            block_count: object.block_count,
                            first_parity_data_ordinal: Some(first_data_ordinal),
                            protected_ordinal_start: None,
                            protected_ordinal_end_exclusive: None,
                            epoch_id: None,
                        })?;
                    }
                }
            }
            TerminalIndexAuthorityRows::ReplayParity {
                checkpoint,
                parity,
                planned_terminal_prefix,
            } => {
                let _retained_checkpoint_authority = checkpoint;
                let mut replay = parity.replay().map_err(|error| {
                    remanence_parity::ParityError::TapeIndexReplica(format!(
                        "replay parity journal rows: {error}"
                    ))
                })?;
                while let Some(entry) = replay.next_entry().map_err(|error| {
                    remanence_parity::ParityError::TapeIndexReplica(format!(
                        "replay parity journal row: {error}"
                    ))
                })? {
                    visitor(&terminal_map_entry_from_committed(&entry)?)?;
                }
                if let Some(prefix) = planned_terminal_prefix {
                    for entry in &prefix.committed_bundle.entries {
                        visitor(&terminal_map_entry_from_committed(entry)?)?;
                    }
                }
            }
            TerminalIndexAuthorityRows::ReplayNoParity(checkpoint) => {
                visitor(&remanence_parity::TapeIndexReplicaMapEntry {
                    tape_file_number: 0,
                    kind: remanence_parity::TapeIndexReplicaFileKind::Bootstrap,
                    block_count: 1,
                    first_parity_data_ordinal: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    epoch_id: None,
                })?;
                checkpoint
                    .visit_records(|record| {
                        for object in &record.objects {
                            let first_data_ordinal = object
                                .total_committed_ordinals
                                .checked_sub(object.block_count)
                                .ok_or_else(|| {
                                    StateError::JournalReplayFailed(
                                        "validated no-parity Object ordinal range underflowed"
                                            .to_string(),
                                    )
                                })?;
                            visitor(&remanence_parity::TapeIndexReplicaMapEntry {
                                tape_file_number: object.copy.tape_file_number,
                                kind: remanence_parity::TapeIndexReplicaFileKind::Object,
                                block_count: object.block_count,
                                first_parity_data_ordinal: Some(first_data_ordinal),
                                protected_ordinal_start: None,
                                protected_ordinal_end_exclusive: None,
                                epoch_id: None,
                            })
                            .map_err(|error| {
                                StateError::JournalReplayFailed(format!(
                                    "terminal structural-row visitor failed: {error}"
                                ))
                            })?;
                        }
                        Ok(())
                    })
                    .map_err(|error| {
                        remanence_parity::ParityError::TapeIndexReplica(error.to_string())
                    })?;
            }
        }
        Ok(())
    }

    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(
            &remanence_parity::TapeIndexReplicaObjectRow,
        ) -> Result<(), remanence_parity::ParityError>,
    ) -> Result<(), remanence_parity::ParityError> {
        match &self.rows {
            TerminalIndexAuthorityRows::Parity(committed) => {
                for entry in &committed.entries {
                    if entry.kind == remanence_parity::TapeFileKind::Object {
                        let row = entry.object_recovery_row.as_ref().ok_or(
                            remanence_parity::ParityError::Invariant(
                                "validated terminal Object entry lost its recovery row",
                            ),
                        )?;
                        visitor(&terminal_object_row_from_recovery(row)?)?;
                    }
                }
            }
            TerminalIndexAuthorityRows::NoParity(records) => {
                for record in *records {
                    for object in &record.objects {
                        visitor(&terminal_object_row_from_recovery(
                            &object.object_recovery_row.to_parity_row(),
                        )?)?;
                    }
                }
            }
            TerminalIndexAuthorityRows::ReplayParity {
                checkpoint,
                parity: _,
                planned_terminal_prefix,
            } => {
                let _retained_terminal_prefix = planned_terminal_prefix;
                checkpoint
                    .visit_records(|record| {
                        for object in &record.objects {
                            let row = terminal_object_row_from_recovery(
                                &object.object_recovery_row.to_parity_row(),
                            )
                            .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
                            visitor(&row).map_err(|error| {
                                StateError::JournalReplayFailed(format!(
                                    "terminal Object-row visitor failed: {error}"
                                ))
                            })?;
                        }
                        Ok(())
                    })
                    .map_err(|error| {
                        remanence_parity::ParityError::TapeIndexReplica(error.to_string())
                    })?;
            }
            TerminalIndexAuthorityRows::ReplayNoParity(checkpoint) => {
                checkpoint
                    .visit_records(|record| {
                        for object in &record.objects {
                            let row = terminal_object_row_from_recovery(
                                &object.object_recovery_row.to_parity_row(),
                            )
                            .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
                            visitor(&row).map_err(|error| {
                                StateError::JournalReplayFailed(format!(
                                    "terminal Object-row visitor failed: {error}"
                                ))
                            })?;
                        }
                        Ok(())
                    })
                    .map_err(|error| {
                        remanence_parity::ParityError::TapeIndexReplica(error.to_string())
                    })?;
            }
        }
        Ok(())
    }
}

fn terminal_map_entry_from_committed(
    entry: &remanence_parity::TapeFileEntry,
) -> Result<remanence_parity::TapeIndexReplicaMapEntry, remanence_parity::ParityError> {
    let kind = match entry.kind {
        remanence_parity::TapeFileKind::Object => {
            remanence_parity::TapeIndexReplicaFileKind::Object
        }
        remanence_parity::TapeFileKind::ParitySidecar => {
            remanence_parity::TapeIndexReplicaFileKind::ParitySidecar
        }
        remanence_parity::TapeFileKind::Bootstrap => {
            remanence_parity::TapeIndexReplicaFileKind::Bootstrap
        }
        remanence_parity::TapeFileKind::ParityMap => {
            remanence_parity::TapeIndexReplicaFileKind::ParityMap
        }
        remanence_parity::TapeFileKind::TapeIndexReplica
        | remanence_parity::TapeFileKind::IndexSeparationExtent => {
            return Err(remanence_parity::ParityError::Invariant(
                "terminal component appears inside the pre-tail authority",
            ));
        }
    };
    Ok(remanence_parity::TapeIndexReplicaMapEntry {
        tape_file_number: entry.tape_file_number,
        kind,
        block_count: entry.block_count,
        first_parity_data_ordinal: entry.first_parity_data_ordinal,
        protected_ordinal_start: entry.protected_ordinal_start,
        protected_ordinal_end_exclusive: entry.protected_ordinal_end_exclusive,
        epoch_id: entry.epoch_id,
    })
}

fn terminal_object_row_from_recovery(
    row: &remanence_parity::ObjectRecoveryRow,
) -> Result<remanence_parity::TapeIndexReplicaObjectRow, remanence_parity::ParityError> {
    let object_id = row
        .object_id
        .clone()
        .ok_or(remanence_parity::ParityError::Invariant(
            "terminal Object recovery row is missing its Object identifier",
        ))?;
    if object_id.is_empty() || object_id.len() > 64 {
        return Err(remanence_parity::ParityError::Invariant(
            "terminal Object recovery identifier is outside 1..=64 bytes",
        ));
    }
    Ok(remanence_parity::TapeIndexReplicaObjectRow {
        tape_file_number: row.tape_file_number,
        stored_block_count: row.stored_block_count,
        object_id,
        representation: row.representation.clone(),
    })
}

fn validate_parity_terminal_index_rows(
    committed: &remanence_parity::CommittedState,
) -> Result<TerminalIndexAuthoritySummary, StateError> {
    if !committed.orphaned_bundles.is_empty() {
        return Err(StateError::JournalReplayFailed(
            "terminal index authority contains unresolved parity orphans".to_string(),
        ));
    }
    let mut object_count = 0u64;
    for (index, entry) in committed.entries.iter().enumerate() {
        let expected = u64::try_from(index).map_err(|_| {
            StateError::JournalReplayFailed(
                "terminal index structural position overflows u64".to_string(),
            )
        })?;
        if entry.tape_file_number != expected {
            return Err(StateError::JournalReplayFailed(format!(
                "terminal index structural map is not dense at {expected}: found {}",
                entry.tape_file_number
            )));
        }
        terminal_map_entry_from_committed(entry)
            .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
        if entry.kind == remanence_parity::TapeFileKind::Object {
            let row = entry.object_recovery_row.as_ref().ok_or_else(|| {
                StateError::JournalReplayFailed(format!(
                    "terminal Object tape file {expected} has no recovery row"
                ))
            })?;
            if row.tape_file_number != entry.tape_file_number
                || row.stored_block_count != entry.block_count
            {
                return Err(StateError::JournalReplayFailed(format!(
                    "terminal Object recovery row does not bind map entry {expected}"
                )));
            }
            terminal_object_row_from_recovery(row)
                .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
            object_count = object_count.checked_add(1).ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "terminal Object recovery-row count overflows u64".to_string(),
                )
            })?;
        } else if entry.object_recovery_row.is_some() {
            return Err(StateError::JournalReplayFailed(format!(
                "non-Object terminal map entry {expected} carries an Object recovery row"
            )));
        }
    }
    let structural_entry_count = u64::try_from(committed.entries.len()).map_err(|_| {
        StateError::JournalReplayFailed("terminal structural-entry count overflows u64".to_string())
    })?;
    if structural_entry_count == 0
        || committed.entries[0].kind != remanence_parity::TapeFileKind::Bootstrap
    {
        return Err(StateError::JournalReplayFailed(
            "terminal index authority is missing its BOT Bootstrap entry".to_string(),
        ));
    }
    Ok(TerminalIndexAuthoritySummary {
        scope: remanence_parity::TapeIndexReplicaScope {
            covered_prefix_tape_file_count: structural_entry_count,
            total_data_ordinals: committed.total_committed_ordinals,
            highest_protected_ordinal: committed.highest_protected_ordinal,
        },
        counts: remanence_parity::TapeIndexReplicaCounts {
            structural_entry_count,
            object_row_count: object_count,
        },
    })
}

fn validate_no_parity_terminal_index_rows(
    records: &[CheckpointJournalRecord],
) -> Result<TerminalIndexAuthoritySummary, StateError> {
    let tape_uuid = records[0].tape_uuid;
    let block_size = records[0].block_size;
    let mut expected_file = 1u64;
    let mut object_count = 0u64;
    let mut total_data_ordinals = 0u64;
    for record in records {
        if record.tape_uuid != tape_uuid
            || record.block_size != block_size
            || record.scheme.is_some()
            || !record.object_tape_file_bundles.is_empty()
            || record.sealed_after_write
        {
            return Err(StateError::JournalReplayFailed(
                "no-parity terminal authority contains mixed or terminal record state".to_string(),
            ));
        }
        for object in &record.objects {
            let object_file = object.copy.tape_file_number;
            if object_file != expected_file
                || object.object_recovery_row.tape_file_number != object_file
                || object.object_recovery_row.stored_block_count != object.block_count
            {
                return Err(StateError::JournalReplayFailed(format!(
                    "no-parity terminal Object row/map mismatch at tape file {expected_file}"
                )));
            }
            terminal_object_row_from_recovery(&object.object_recovery_row.to_parity_row())
                .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
            expected_file = expected_file.checked_add(1).ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "no-parity terminal tape-file number overflows u64".to_string(),
                )
            })?;
            object_count = object_count.checked_add(1).ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "no-parity terminal Object-row count overflows u64".to_string(),
                )
            })?;
            total_data_ordinals = object.total_committed_ordinals;
        }
        if record.next_tape_file_number != expected_file {
            return Err(StateError::JournalReplayFailed(format!(
                "no-parity checkpoint next boundary is {}, expected {expected_file}",
                record.next_tape_file_number
            )));
        }
    }
    Ok(TerminalIndexAuthoritySummary {
        scope: remanence_parity::TapeIndexReplicaScope {
            covered_prefix_tape_file_count: expected_file,
            total_data_ordinals,
            highest_protected_ordinal: 0,
        },
        counts: remanence_parity::TapeIndexReplicaCounts {
            structural_entry_count: expected_file,
            object_row_count: object_count,
        },
    })
}

fn observe_replayed_parity_terminal_entry(
    entry: &remanence_parity::TapeFileEntry,
    expected_file: &mut u64,
    object_count: &mut u64,
) -> Result<(), StateError> {
    if entry.tape_file_number != *expected_file {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal index structural map is not dense at {}: found {}",
            *expected_file, entry.tape_file_number
        )));
    }
    terminal_map_entry_from_committed(entry)
        .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
    if entry.kind == remanence_parity::TapeFileKind::Object {
        if let Some(row) = entry.object_recovery_row.as_ref() {
            if row.tape_file_number != entry.tape_file_number
                || row.stored_block_count != entry.block_count
            {
                return Err(StateError::JournalReplayFailed(format!(
                    "terminal Object recovery row does not bind map entry {}",
                    *expected_file
                )));
            }
            terminal_object_row_from_recovery(row)
                .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
        }
        *object_count = object_count.checked_add(1).ok_or_else(|| {
            StateError::JournalReplayFailed(
                "terminal Object recovery-row count overflows u64".to_string(),
            )
        })?;
    } else if entry.object_recovery_row.is_some() {
        return Err(StateError::JournalReplayFailed(format!(
            "non-Object terminal map entry {} carries an Object recovery row",
            *expected_file
        )));
    }
    *expected_file = expected_file.checked_add(1).ok_or_else(|| {
        StateError::JournalReplayFailed("terminal structural count overflows u64".to_string())
    })?;
    Ok(())
}

fn validate_replay_backed_parity_terminal_rows(
    checkpoint: &CheckpointReplaySnapshot,
    parity: &remanence_parity::FileTapeFileJournalCommittedSnapshot,
    terminal_prefix: ReplayTerminalPrefixMode<'_>,
) -> Result<(TerminalIndexAuthoritySummary, [u8; 16]), StateError> {
    let mismatch = |detail: String| {
        StateError::JournalReplayFailed(format!(
            "bounded parity terminal authority mismatch: {detail}"
        ))
    };
    let mut replay = parity
        .replay()
        .map_err(|error| mismatch(error.to_string()))?;
    let mut expected_file = 0u64;
    let mut object_count = 0u64;
    let bot = replay
        .next_entry()
        .map_err(|error| mismatch(error.to_string()))?
        .ok_or_else(|| mismatch("sink journal has no committed BOT Bootstrap".to_string()))?;
    if bot.tape_file_number != 0
        || bot.kind != remanence_parity::TapeFileKind::Bootstrap
        || bot.block_count != 1
    {
        return Err(mismatch(format!(
            "sink journal does not start with the one-block BOT Bootstrap: {bot:?}"
        )));
    }
    observe_replayed_parity_terminal_entry(&bot, &mut expected_file, &mut object_count)?;

    let replayed_checkpoint = checkpoint.visit_records(|record| {
        if record.scheme.as_ref() != Some(parity.scheme()) {
            return Err(mismatch(
                "checkpoint parity scheme does not match the sink journal".to_string(),
            ));
        }
        for expected in record
            .object_tape_file_bundles
            .iter()
            .flat_map(|bundle| bundle.entries.iter())
            .chain(
                record
                    .barrier_bundle
                    .iter()
                    .flat_map(|bundle| bundle.entries.iter()),
            )
        {
            let actual = replay
                .next_entry()
                .map_err(|error| mismatch(error.to_string()))?
                .ok_or_else(|| {
                    mismatch(format!(
                        "sink journal ends before checkpoint entry {}",
                        expected.tape_file_number
                    ))
                })?;
            if !parity_resume_entries_match(&actual, expected) {
                return Err(mismatch(format!(
                    "checkpoint and sink prefixes differ at entry {}: checkpoint={expected:?}, sink={actual:?}",
                    expected.tape_file_number
                )));
            }
            observe_replayed_parity_terminal_entry(
                &actual,
                &mut expected_file,
                &mut object_count,
            )?;
        }
        Ok(())
    })?;
    let last = replayed_checkpoint.last.ok_or_else(|| {
        mismatch("terminal index authority has no committed checkpoint prefix".to_string())
    })?;
    if last.sealed_after_write {
        return Err(mismatch(
            "terminal index authority already ends in sealed checkpoint state".to_string(),
        ));
    }
    if last.tape_uuid != checkpoint.tape_uuid || last.tape_uuid != parity.tape_uuid() {
        return Err(mismatch(
            "checkpoint and parity journals name different tapes".to_string(),
        ));
    }
    if last.block_size != parity.block_size() {
        return Err(mismatch(format!(
            "checkpoint block size {} does not match sink journal block size {}",
            last.block_size,
            parity.block_size()
        )));
    }
    let layout = validate_parity_barrier_bundles(&last)
        .map_err(|error| mismatch(format!("final checkpoint record is invalid: {error}")))?;
    let expected_eod_lba = layout
        .last_tape_file
        .physical_start_hint
        .ok_or_else(|| mismatch("checkpoint final row has no physical start hint".to_string()))?
        .checked_add(layout.last_tape_file.block_count)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| mismatch("checkpoint EOD calculation overflows".to_string()))?;
    if last.eod_partition != 0 || last.eod_lba != expected_eod_lba {
        return Err(mismatch(format!(
            "checkpoint barrier position is partition {} lba {}, expected partition 0 lba {expected_eod_lba}",
            last.eod_partition, last.eod_lba
        )));
    }

    let prefix = match terminal_prefix {
        ReplayTerminalPrefixMode::Absent => None,
        ReplayTerminalPrefixMode::Planned(prefix) | ReplayTerminalPrefixMode::Durable(prefix) => {
            Some(prefix)
        }
    };
    let (expected_highest, expected_total) = if let Some(prefix) = prefix {
        prefix.validate()?;
        let expected_prefix_file = last.next_tape_file_number;
        if prefix.start_tape_file_number != expected_prefix_file || prefix.start_lba != last.eod_lba
        {
            return Err(mismatch(format!(
                "persisted TerminalPrefix starts at tape file {}/LBA {}, expected {expected_prefix_file}/{}",
                prefix.start_tape_file_number, prefix.start_lba, last.eod_lba
            )));
        }
        if prefix.committed_bundle.total_committed_ordinals != layout.total_committed_ordinals
            || prefix.committed_bundle.highest_protected_ordinal < layout.highest_protected_ordinal
        {
            return Err(mismatch(
                "persisted TerminalPrefix does not extend checkpoint watermarks".to_string(),
            ));
        }
        for expected in &prefix.committed_bundle.entries {
            match terminal_prefix {
                ReplayTerminalPrefixMode::Planned(_) => {
                    observe_replayed_parity_terminal_entry(
                        expected,
                        &mut expected_file,
                        &mut object_count,
                    )?;
                }
                ReplayTerminalPrefixMode::Durable(_) => {
                    let actual = replay
                        .next_entry()
                        .map_err(|error| mismatch(error.to_string()))?
                        .ok_or_else(|| {
                            mismatch(format!(
                                "sink journal ends before TerminalPrefix entry {}",
                                expected.tape_file_number
                            ))
                        })?;
                    if actual != *expected {
                        return Err(mismatch(format!(
                            "persisted TerminalPrefix differs at entry {}: planned={expected:?}, sink={actual:?}",
                            expected.tape_file_number
                        )));
                    }
                    observe_replayed_parity_terminal_entry(
                        &actual,
                        &mut expected_file,
                        &mut object_count,
                    )?;
                }
                ReplayTerminalPrefixMode::Absent => unreachable!("prefix mode has a plan"),
            }
        }
        (
            prefix.committed_bundle.highest_protected_ordinal,
            prefix.committed_bundle.total_committed_ordinals,
        )
    } else {
        (
            layout.highest_protected_ordinal,
            layout.total_committed_ordinals,
        )
    };
    if replay
        .next_entry()
        .map_err(|error| mismatch(error.to_string()))?
        .is_some()
    {
        return Err(mismatch(
            "sink journal contains committed rows beyond selected terminal authority".to_string(),
        ));
    }
    let expected_replayed_entry_count = match terminal_prefix {
        ReplayTerminalPrefixMode::Planned(prefix) => replay
            .committed_entry_count()
            .checked_add(
                u64::try_from(prefix.committed_bundle.entries.len()).map_err(|_| {
                    mismatch("planned TerminalPrefix entry count exceeds u64".to_string())
                })?,
            )
            .ok_or_else(|| mismatch("planned structural entry count overflows u64".to_string()))?,
        ReplayTerminalPrefixMode::Absent | ReplayTerminalPrefixMode::Durable(_) => {
            replay.committed_entry_count()
        }
    };
    let (expected_replay_highest, expected_replay_total) = match terminal_prefix {
        ReplayTerminalPrefixMode::Planned(_) => (
            layout.highest_protected_ordinal,
            layout.total_committed_ordinals,
        ),
        ReplayTerminalPrefixMode::Absent | ReplayTerminalPrefixMode::Durable(_) => {
            (expected_highest, expected_total)
        }
    };
    if expected_file != expected_replayed_entry_count
        || object_count != last.committed_object_count
        || replay.highest_protected_ordinal() != expected_replay_highest
        || replay.total_committed_ordinals() != expected_replay_total
    {
        return Err(mismatch(format!(
            "summary disagreement: rows {expected_file}/{expected_replayed_entry_count}, objects {object_count}/{}, replay W {}/{expected_replay_highest}, replay T {}/{expected_replay_total}",
            last.committed_object_count,
            replay.highest_protected_ordinal(),
            replay.total_committed_ordinals()
        )));
    }
    Ok((
        TerminalIndexAuthoritySummary {
            scope: remanence_parity::TapeIndexReplicaScope {
                covered_prefix_tape_file_count: expected_file,
                total_data_ordinals: expected_total,
                highest_protected_ordinal: expected_highest,
            },
            counts: remanence_parity::TapeIndexReplicaCounts {
                structural_entry_count: expected_file,
                object_row_count: object_count,
            },
        },
        last.tape_uuid,
    ))
}

fn validate_replay_backed_no_parity_terminal_rows(
    checkpoint: &CheckpointReplaySnapshot,
) -> Result<(TerminalIndexAuthoritySummary, [u8; 16]), StateError> {
    let mut expected_file = 1u64;
    let mut object_count = 0u64;
    let mut total_data_ordinals = 0u64;
    let replayed_checkpoint = checkpoint.visit_records(|record| {
        if record.scheme.is_some()
            || !record.object_tape_file_bundles.is_empty()
            || record.barrier_bundle.is_some()
            || record.sealed_after_write
        {
            return Err(StateError::JournalReplayFailed(
                "no-parity terminal authority contains parity or terminal state".to_string(),
            ));
        }
        for object in &record.objects {
            if object.copy.tape_file_number != expected_file
                || object.object_recovery_row.tape_file_number != expected_file
                || object.object_recovery_row.stored_block_count != object.block_count
            {
                return Err(StateError::JournalReplayFailed(format!(
                    "no-parity terminal Object row/map mismatch at tape file {expected_file}"
                )));
            }
            terminal_object_row_from_recovery(&object.object_recovery_row.to_parity_row())
                .map_err(|error| StateError::JournalReplayFailed(error.to_string()))?;
            expected_file = expected_file.checked_add(1).ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "no-parity terminal tape-file number overflows u64".to_string(),
                )
            })?;
            object_count = object_count.checked_add(1).ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "no-parity terminal Object-row count overflows u64".to_string(),
                )
            })?;
            total_data_ordinals = object.total_committed_ordinals;
        }
        if record.next_tape_file_number != expected_file {
            return Err(StateError::JournalReplayFailed(format!(
                "no-parity checkpoint next boundary is {}, expected {expected_file}",
                record.next_tape_file_number
            )));
        }
        Ok(())
    })?;
    let last = replayed_checkpoint.last.ok_or_else(|| {
        StateError::JournalReplayFailed(
            "terminal index authority has no committed checkpoint prefix".to_string(),
        )
    })?;
    if object_count != last.committed_object_count {
        return Err(StateError::JournalReplayFailed(format!(
            "no-parity replay counted {object_count} objects, checkpoint reports {}",
            last.committed_object_count
        )));
    }
    Ok((
        TerminalIndexAuthoritySummary {
            scope: remanence_parity::TapeIndexReplicaScope {
                covered_prefix_tape_file_count: expected_file,
                total_data_ordinals,
                highest_protected_ordinal: 0,
            },
            counts: remanence_parity::TapeIndexReplicaCounts {
                structural_entry_count: expected_file,
                object_row_count: object_count,
            },
        },
        last.tape_uuid,
    ))
}

#[derive(Debug, Deserialize, Serialize)]
struct CheckpointJournalFrame {
    records: Vec<CheckpointJournalRecord>,
}

/// Append-only per-tape checkpoint journal.
#[derive(Debug)]
pub struct FileCheckpointJournal {
    path: PathBuf,
    tape_uuid: [u8; 16],
}

/// Exclusive per-tape checkpoint authority retained across replay, media I/O,
/// and the next durable checkpoint append.
#[derive(Debug)]
pub struct FileCheckpointJournalLease {
    path: PathBuf,
    tape_uuid: [u8; 16],
    file: File,
    _lock: Flock<File>,
}

/// Deterministic allocation and pass counters for bounded checkpoint replay.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CheckpointBoundedReplayMetrics {
    /// Complete journal passes performed.
    pub replay_passes: u64,
    /// Number of length-and-integrity-protected frames decoded.
    pub frame_count: u64,
    /// Largest encoded frame payload decoded in bytes.
    pub peak_frame_payload_bytes: u64,
    /// Largest number of checkpoint records simultaneously retained.
    pub peak_live_record_count: u64,
    /// Largest number of Object projections simultaneously retained.
    pub peak_live_object_rows: u64,
}

#[derive(Debug)]
struct BoundedCheckpointReplay {
    last: Option<CheckpointJournalRecord>,
    metrics: CheckpointBoundedReplayMetrics,
}

/// Frozen checkpoint-journal prefix used by replay-backed terminal sources.
#[derive(Debug)]
struct CheckpointReplaySnapshot {
    file: File,
    path: PathBuf,
    tape_uuid: [u8; 16],
    replay_end: u64,
    metrics: CheckpointBoundedReplayMetrics,
}

impl CheckpointReplaySnapshot {
    fn visit_records(
        &self,
        visitor: impl FnMut(&CheckpointJournalRecord) -> Result<(), StateError>,
    ) -> Result<BoundedCheckpointReplay, StateError> {
        let mut file = self.file.try_clone().map_err(|error| {
            StateError::io_at(
                "clone frozen checkpoint journal for replay",
                &self.path,
                error,
            )
        })?;
        if file
            .metadata()
            .map_err(|error| {
                StateError::io_at("stat frozen checkpoint journal", &self.path, error)
            })?
            .len()
            < self.replay_end
        {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint journal {} was truncated before frozen boundary {}",
                self.path.display(),
                self.replay_end
            )));
        }
        visit_checkpoint_records_bounded_to(
            &mut file,
            self.tape_uuid,
            &self.path,
            self.replay_end,
            visitor,
        )
    }
}

/// Ordinary checkpoint prefix and pending structured intent loaded under the
/// exclusive terminal-recovery lease without admitting Object append.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointTerminalRecoveryAuthority {
    /// CRC-validated checkpoint authority, including a completion if present.
    pub records: Vec<CheckpointJournalRecord>,
    /// Pending structured terminal-finalization intent.
    pub finalization_intent: Option<TerminalFinalizationIntent>,
}

impl FileCheckpointJournal {
    /// Open or create the journal handle for `tape_uuid` beneath `dir`.
    pub fn open(dir: impl AsRef<Path>, tape_uuid: [u8; 16]) -> Result<Self, StateError> {
        let dir = dir.as_ref();
        let created_dir = !dir.exists();
        fs::create_dir_all(dir)
            .map_err(|err| StateError::io_at("create checkpoint journal directory", dir, err))?;
        if created_dir {
            let parent = dir.parent().ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "checkpoint journal directory has no parent".to_string(),
                )
            })?;
            File::open(parent)
                .and_then(|parent| parent.sync_all())
                .map_err(|err| {
                    StateError::io_at("fsync checkpoint journal parent directory", parent, err)
                })?;
        }
        let path = checkpoint_journal_path(dir, tape_uuid);
        remanence_parity::validate_trusted_journal_volume(&path).map_err(|err| match err {
            remanence_parity::JournalError::UntrustedVolume(detail) => {
                StateError::UntrustedStateVolume(detail)
            }
            other => StateError::JournalReplayFailed(format!(
                "checkpoint trusted-volume validation failed: {other}"
            )),
        })?;
        Ok(Self { path, tape_uuid })
    }

    /// Acquire the exclusive lease that write paths retain from authority
    /// replay through media work and checkpoint append.
    pub fn acquire_exclusive(&self) -> Result<FileCheckpointJournalLease, StateError> {
        self.acquire_exclusive_inner(false)
    }

    /// Acquire an exclusive lease for recovery of a pending structured
    /// terminal-finalization intent.
    pub fn acquire_exclusive_for_terminal_recovery(
        &self,
    ) -> Result<FileCheckpointJournalLease, StateError> {
        self.acquire_exclusive_inner(true)
    }

    fn acquire_exclusive_inner(
        &self,
        allow_pending_terminal_intent: bool,
    ) -> Result<FileCheckpointJournalLease, StateError> {
        let lock = acquire_checkpoint_lock(
            &self.path,
            FlockArg::LockExclusiveNonblock,
            "lock checkpoint journal for write session",
        )?;
        let file = if self.path.exists() {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path)
                .map_err(|err| StateError::io_at("open checkpoint journal", &self.path, err))?
        } else {
            initialize_checkpoint_file(&self.path, self.tape_uuid)?
        };
        let lease = FileCheckpointJournalLease {
            path: self.path.clone(),
            tape_uuid: self.tape_uuid,
            file,
            _lock: lock,
        };
        lease.validate_bounded_acquisition(allow_pending_terminal_intent)?;
        Ok(lease)
    }

    /// Append and fsync one validated checkpoint record under a short-lived
    /// exclusive lease. Production write paths should retain a lease across
    /// their replay and media work instead.
    pub fn append(&self, record: &CheckpointJournalRecord) -> Result<(), StateError> {
        self.acquire_exclusive()?.append(record)
    }

    /// Replay every record, failing closed on a torn final frame.
    pub fn replay(&self) -> Result<Vec<CheckpointJournalRecord>, StateError> {
        let _lock = acquire_checkpoint_lock(
            &self.path,
            FlockArg::LockSharedNonblock,
            "lock checkpoint journal for replay",
        )?;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let records = Vec::new();
                enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &records, false)?;
                return Ok(records);
            }
            Err(err) => {
                return Err(StateError::io_at(
                    "open checkpoint journal",
                    &self.path,
                    err,
                ));
            }
        };
        let records = replay_checkpoint_records(&mut file, self.tape_uuid, &self.path)?;
        enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &records, false)?;
        Ok(records)
    }

    /// Return the final fsynced checkpoint, if any.
    pub fn last(&self) -> Result<Option<CheckpointJournalRecord>, StateError> {
        let _lock = acquire_checkpoint_lock(
            &self.path,
            FlockArg::LockSharedNonblock,
            "lock checkpoint journal for final-record replay",
        )?;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &[], false)?;
                return Ok(None);
            }
            Err(error) => {
                return Err(StateError::io_at(
                    "open checkpoint journal",
                    &self.path,
                    error,
                ));
            }
        };
        let mut tail = Vec::with_capacity(2);
        visit_checkpoint_records_bounded(&mut file, self.tape_uuid, &self.path, |record| {
            if tail.len() == 2 {
                tail.remove(0);
            }
            tail.push(record.clone());
            Ok(())
        })?;
        enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &tail, false)?;
        Ok(tail.pop())
    }

    /// Filesystem path used by this journal.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read a structured terminal-finalization intent without admitting append.
    pub fn terminal_finalization_intent(
        &self,
    ) -> Result<Option<TerminalFinalizationIntent>, StateError> {
        let _lock = acquire_checkpoint_lock(
            &self.path,
            FlockArg::LockSharedNonblock,
            "lock checkpoint journal for finalization-intent read",
        )?;
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let intent = read_terminal_finalization_intent(&self.path, self.tape_uuid)?;
                if let Some(intent) = &intent {
                    intent.validate_for_checkpoint_prefix(None)?;
                }
                return Ok(intent);
            }
            Err(error) => {
                return Err(StateError::io_at(
                    "open checkpoint journal",
                    &self.path,
                    error,
                ));
            }
        };
        let mut records = Vec::with_capacity(2);
        visit_checkpoint_records_bounded(&mut file, self.tape_uuid, &self.path, |record| {
            if records.len() == 2 {
                records.remove(0);
            }
            records.push(record.clone());
            Ok(())
        })?;
        let intent = read_terminal_finalization_intent(&self.path, self.tape_uuid)?;
        if let Some(intent) = &intent {
            intent.validate_for_checkpoint_prefix(finalization_base_record(&records))?;
        }
        Ok(intent)
    }
}

impl FileCheckpointJournalLease {
    fn bounded_replay_snapshot(&self) -> Result<CheckpointReplaySnapshot, StateError> {
        let file = File::open(&self.path).map_err(|error| {
            StateError::io_at(
                "open checkpoint journal for frozen terminal replay",
                &self.path,
                error,
            )
        })?;
        let replay_end = file
            .metadata()
            .map_err(|error| StateError::io_at("stat checkpoint journal", &self.path, error))?
            .len();
        let snapshot = CheckpointReplaySnapshot {
            file,
            path: self.path.clone(),
            tape_uuid: self.tape_uuid,
            replay_end,
            metrics: CheckpointBoundedReplayMetrics::default(),
        };
        let replay = snapshot.visit_records(|_| Ok(()))?;
        Ok(CheckpointReplaySnapshot {
            metrics: replay.metrics,
            ..snapshot
        })
    }

    fn bounded_tail_records(&self) -> Result<Vec<CheckpointJournalRecord>, StateError> {
        let mut tail = Vec::with_capacity(2);
        self.visit_records_bounded(|record| {
            if tail.len() == 2 {
                tail.remove(0);
            }
            tail.push(record.clone());
            Ok(())
        })?;
        Ok(tail)
    }

    fn validate_bounded_acquisition(
        &self,
        allow_pending_terminal_intent: bool,
    ) -> Result<(), StateError> {
        let tail = self.bounded_tail_records()?;
        if !allow_pending_terminal_intent {
            return enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &tail, true);
        }
        let finalization_intent = read_terminal_finalization_intent(&self.path, self.tape_uuid)?;
        if finalization_intent.is_none() {
            return Err(StateError::JournalReplayFailed(
                "terminal recovery lease requested without a pending finalization intent"
                    .to_string(),
            ));
        }
        if let Some(intent) = &finalization_intent {
            intent.validate_for_checkpoint_prefix(finalization_base_record(&tail))?;
            if let Some(completion) = tail
                .last()
                .and_then(|record| record.terminal_finalization.as_ref())
            {
                if completion != intent {
                    return Err(StateError::JournalReplayFailed(
                        "sealed completion differs from pending terminal recovery intent"
                            .to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Replay CRC-validated checkpoint records with a bounded callback.
    ///
    /// This retains the exclusive lease and decodes at most one 64-MiB-bounded
    /// journal frame at a time. The complete checkpoint history is never
    /// accumulated in memory.
    fn visit_records_bounded(
        &self,
        visitor: impl FnMut(&CheckpointJournalRecord) -> Result<(), StateError>,
    ) -> Result<BoundedCheckpointReplay, StateError> {
        let mut file = self.file.try_clone().map_err(|error| {
            StateError::io_at(
                "clone checkpoint journal for bounded replay",
                &self.path,
                error,
            )
        })?;
        visit_checkpoint_records_bounded(&mut file, self.tape_uuid, &self.path, visitor)
    }

    /// Visit every CRC-validated record while retaining only one bounded
    /// journal frame and the caller's own callback state.
    ///
    /// This is the production projection seam for owners that must rebuild a
    /// cache from the complete authority without retaining checkpoint history.
    /// The returned metrics describe this single replay pass.
    pub fn for_each_record_bounded(
        &self,
        visitor: impl FnMut(&CheckpointJournalRecord) -> Result<(), StateError>,
    ) -> Result<CheckpointBoundedReplayMetrics, StateError> {
        Ok(self.visit_records_bounded(visitor)?.metrics)
    }

    /// Return only the final CRC-validated checkpoint while retaining the
    /// exclusive lease. The complete history is streamed and discarded.
    pub fn last_record_bounded(&self) -> Result<Option<CheckpointJournalRecord>, StateError> {
        Ok(self.visit_records_bounded(|_| Ok(()))?.last)
    }

    /// Measure one no-all-history checkpoint replay without retaining its rows.
    pub fn bounded_replay_metrics(&self) -> Result<CheckpointBoundedReplayMetrics, StateError> {
        Ok(self.visit_records_bounded(|_| Ok(()))?.metrics)
    }

    /// Replay the authority while retaining the exclusive lease.
    pub fn replay(&mut self) -> Result<Vec<CheckpointJournalRecord>, StateError> {
        let records = replay_checkpoint_records(&mut self.file, self.tape_uuid, &self.path)?;
        enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &records, true)?;
        Ok(records)
    }

    /// Replay authority while deliberately preserving an unsealed terminal fence.
    ///
    /// This is the only replay seam for a recovery owner that has acquired
    /// [`FileCheckpointJournal::acquire_exclusive_for_terminal_recovery`]. It
    /// validates the intent against the ordinary checkpoint prefix but does
    /// not treat the expected pending/unsealed state as an append error. If a
    /// matching sealed completion is already durable, it wins and the stale
    /// companion intent is cleared before returning.
    pub fn replay_for_terminal_recovery(
        &mut self,
    ) -> Result<CheckpointTerminalRecoveryAuthority, StateError> {
        let records = replay_checkpoint_records(&mut self.file, self.tape_uuid, &self.path)?;
        let finalization_intent = read_terminal_finalization_intent(&self.path, self.tape_uuid)?;
        if finalization_intent.is_none() {
            return Err(StateError::JournalReplayFailed(
                "terminal recovery lease requested without a pending finalization intent"
                    .to_string(),
            ));
        }
        let mut finalization_intent = finalization_intent;
        if let Some(intent) = &finalization_intent {
            intent.validate_for_checkpoint_prefix(finalization_base_record(&records))?;
            if let Some(completion) = records
                .last()
                .and_then(|record| record.terminal_finalization.as_ref())
            {
                if completion != intent {
                    return Err(StateError::JournalReplayFailed(
                        "sealed completion differs from pending terminal recovery intent"
                            .to_string(),
                    ));
                }
                clear_terminal_finalization_intent(&self.path)?;
                finalization_intent = None;
            }
        }
        Ok(CheckpointTerminalRecoveryAuthority {
            records,
            finalization_intent,
        })
    }

    /// Atomically publish manual/automatic terminal admission and its exact plan.
    ///
    /// An identical existing intent is an idempotent join. Any different
    /// request remains fenced and is reported as an idempotency conflict.
    pub fn begin_terminal_finalization(
        &mut self,
        intent: &TerminalFinalizationIntent,
    ) -> Result<TerminalFinalizationIntent, StateError> {
        intent.validate_for_tape(self.tape_uuid)?;
        if intent.progress != TerminalFinalizationProgress::BeforeReplicaA {
            return Err(StateError::JournalReplayFailed(
                "new terminal finalization must begin at BeforeReplicaA".to_string(),
            ));
        }
        if intent.recovery_required {
            return Err(StateError::JournalReplayFailed(
                "new terminal finalization cannot begin recovery-required".to_string(),
            ));
        }
        let records = self.bounded_tail_records()?;
        if records
            .last()
            .is_some_and(|record| record.sealed_after_write)
        {
            return Err(StateError::JournalReplayFailed(
                "cannot begin terminal finalization after sealed authority".to_string(),
            ));
        }
        intent.validate_for_checkpoint_prefix(records.last())?;
        if let Some(existing) = read_terminal_finalization_intent(&self.path, self.tape_uuid)? {
            if existing == *intent {
                return Ok(existing);
            }
            return Err(StateError::IdempotencyConflict(
                "terminal finalization intent already exists with a different request or plan"
                    .to_string(),
            ));
        }
        write_terminal_finalization_intent(&self.path, intent, false)?;
        Ok(intent.clone())
    }

    /// Read the current structured intent while retaining exclusive ownership.
    pub fn terminal_finalization_intent(
        &self,
    ) -> Result<Option<TerminalFinalizationIntent>, StateError> {
        let records = self.bounded_tail_records()?;
        let intent = read_terminal_finalization_intent(&self.path, self.tape_uuid)?;
        if let Some(intent) = &intent {
            intent.validate_for_checkpoint_prefix(finalization_base_record(&records))?;
        }
        Ok(intent)
    }

    /// Advance exactly one barrier-proved terminal component.
    ///
    /// Repeating an already-published transition is idempotent. Skips,
    /// regressions, and transitions against a different current state fail.
    pub fn advance_terminal_finalization(
        &mut self,
        expected: TerminalFinalizationProgress,
        next: TerminalFinalizationProgress,
    ) -> Result<TerminalFinalizationIntent, StateError> {
        let mut intent = read_terminal_finalization_intent(&self.path, self.tape_uuid)?
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "terminal finalization progress update has no durable intent".to_string(),
                )
            })?;
        if intent.progress == next {
            return Ok(intent);
        }
        if intent.progress != expected || expected.successor() != Some(next) {
            return Err(StateError::JournalReplayFailed(format!(
                "invalid terminal finalization progress transition: durable={:?}, expected={expected:?}, next={next:?}",
                intent.progress
            )));
        }
        intent.progress = next;
        intent.recovery_required = false;
        intent.validate_for_tape(self.tape_uuid)?;
        write_terminal_finalization_intent(&self.path, &intent, true)?;
        Ok(intent)
    }

    /// Durably retain the current terminal boundary as recovery-required.
    ///
    /// Repeating the classification is idempotent. A later successful
    /// component transition clears it while advancing progress; it is never
    /// cleared at the same progress merely because the daemon restarted.
    pub fn mark_terminal_recovery_required(
        &mut self,
    ) -> Result<TerminalFinalizationIntent, StateError> {
        let mut intent = read_terminal_finalization_intent(&self.path, self.tape_uuid)?
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "terminal recovery classification has no durable intent".to_string(),
                )
            })?;
        if intent.recovery_required {
            return Ok(intent);
        }
        intent.recovery_required = true;
        intent.validate_for_tape(self.tape_uuid)?;
        write_terminal_finalization_intent(&self.path, &intent, true)?;
        Ok(intent)
    }

    /// Clear a recovery classification after the complete terminal tail is
    /// already barrier-proved by durable progress.
    ///
    /// This is deliberately narrower than a general recovery-state clear:
    /// before replica C, only a successful successor transition may clear the
    /// classification. At `AfterReplicaC`, no successor exists and the final
    /// checkpoint is entirely host-side work.
    pub fn clear_terminal_recovery_required_after_replica_c(
        &mut self,
    ) -> Result<TerminalFinalizationIntent, StateError> {
        let mut intent = read_terminal_finalization_intent(&self.path, self.tape_uuid)?
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "terminal recovery clear has no durable intent".to_string(),
                )
            })?;
        if intent.progress != TerminalFinalizationProgress::AfterReplicaC {
            return Err(StateError::JournalReplayFailed(format!(
                "terminal recovery clear requires AfterReplicaC, found {:?}",
                intent.progress
            )));
        }
        if !intent.recovery_required {
            return Ok(intent);
        }
        intent.recovery_required = false;
        intent.validate_for_tape(self.tape_uuid)?;
        write_terminal_finalization_intent(&self.path, &intent, true)?;
        Ok(intent)
    }

    /// Append final checkpoint authority after all five components are proved.
    pub fn append_terminal_finalization(
        &mut self,
        records: &[CheckpointJournalRecord],
    ) -> Result<(), StateError> {
        self.append_terminal_finalization_with_after_fsync(records, || Ok(()))
    }

    /// Append final checkpoint authority, run one callback after its fsync,
    /// then retire the matching companion intent.
    ///
    /// The callback exists so an explicitly gated crash harness can exercise
    /// the real sealed-fsync-to-intent-cleanup boundary without exposing a
    /// normal call path that can forget cleanup.
    pub fn append_terminal_finalization_with_after_fsync(
        &mut self,
        records: &[CheckpointJournalRecord],
        after_fsync: impl FnOnce() -> Result<(), StateError>,
    ) -> Result<(), StateError> {
        let intent =
            read_terminal_finalization_intent(&self.path, self.tape_uuid)?.ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "terminal checkpoint transition has no structured finalization intent"
                        .to_string(),
                )
            })?;
        if intent.progress != TerminalFinalizationProgress::AfterReplicaC {
            return Err(StateError::JournalReplayFailed(format!(
                "terminal checkpoint authority requires AfterReplicaC, found {:?}",
                intent.progress
            )));
        }
        if intent.recovery_required {
            return Err(StateError::JournalReplayFailed(
                "terminal checkpoint completion cannot retain recovery-required intent".to_string(),
            ));
        }
        if !records
            .last()
            .is_some_and(|record| record.sealed_after_write)
        {
            return Err(StateError::JournalReplayFailed(
                "terminal checkpoint transition does not end in sealed authority".to_string(),
            ));
        }
        if records
            .last()
            .and_then(|record| record.terminal_finalization.as_ref())
            != Some(&intent)
        {
            return Err(StateError::JournalReplayFailed(
                "terminal checkpoint completion does not match the durable finalization intent"
                    .to_string(),
            ));
        }
        self.append_batch_inner(records, true)?;
        after_fsync()?;
        clear_terminal_finalization_intent(&self.path)
    }

    /// Validate, append, and fsync one checkpoint while retaining the lease.
    pub fn append(&mut self, record: &CheckpointJournalRecord) -> Result<(), StateError> {
        self.append_batch(std::slice::from_ref(record))
    }

    /// Validate and fsync one indivisible ordered checkpoint transition.
    ///
    /// A watermark seal uses this to place the ordinary object checkpoint and
    /// its terminal-only seal authority in one length-and-integrity-protected
    /// frame. Replay therefore observes both records or fails closed on a torn
    /// frame; it cannot publish only the ordinary half.
    pub fn append_batch(&mut self, records: &[CheckpointJournalRecord]) -> Result<(), StateError> {
        self.append_batch_inner(records, false)
    }

    fn append_batch_inner(
        &mut self,
        records: &[CheckpointJournalRecord],
        terminal_transition: bool,
    ) -> Result<(), StateError> {
        if records.is_empty() {
            return Err(StateError::JournalReplayFailed(
                "checkpoint journal frame must contain at least one record".to_string(),
            ));
        }
        if !terminal_transition && records.iter().any(|record| record.sealed_after_write) {
            return Err(StateError::JournalReplayFailed(
                "sealed checkpoint authority requires a structured terminal finalization intent"
                    .to_string(),
            ));
        }
        let prior = self.bounded_tail_records()?;
        if !terminal_transition {
            enforce_terminal_intent_for_replay(&self.path, self.tape_uuid, &prior, true)?;
        }
        let mut previous = prior.last();
        for record in records {
            if record.tape_uuid != self.tape_uuid {
                return Err(StateError::JournalReplayFailed(
                    "checkpoint record tape_uuid does not match journal".to_string(),
                ));
            }
            validate_next_record(previous, record)?;
            previous = Some(record);
        }
        let payload = serde_json::to_vec(&CheckpointJournalFrame {
            records: records.to_vec(),
        })
        .map_err(|err| {
            StateError::JournalReplayFailed(format!("encode checkpoint journal frame: {err}"))
        })?;
        let payload_len = u32::try_from(payload.len()).map_err(|_| {
            StateError::JournalReplayFailed("checkpoint record length does not fit u32".to_string())
        })?;
        if u64::from(payload_len) > MAX_CHECKPOINT_RECORD_LEN {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record length {payload_len} exceeds replay limit {MAX_CHECKPOINT_RECORD_LEN}"
            )));
        }
        let mut frame = Vec::with_capacity(
            usize::try_from(CHECKPOINT_RECORD_PREFIX_LEN)
                .expect("checkpoint record prefix length fits usize")
                .checked_add(payload.len())
                .and_then(|len| len.checked_add(8))
                .ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "checkpoint record frame length overflows usize".to_string(),
                    )
                })?,
        );
        frame.extend_from_slice(&CHECKPOINT_RECORD_VERSION.to_le_bytes());
        frame.extend_from_slice(&payload_len.to_le_bytes());
        frame.extend_from_slice(&payload);
        let crc = remanence_parity::crc64_xz(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());

        let append_start = self
            .file
            .seek(SeekFrom::End(0))
            .map_err(|err| StateError::io_at("seek checkpoint journal", &self.path, err))?;
        if let Err(err) = self
            .file
            .write_all(&frame)
            .and_then(|_| self.file.sync_all())
        {
            let rollback = self
                .file
                .set_len(append_start)
                .and_then(|_| self.file.sync_all());
            if let Err(rollback_err) = rollback {
                return Err(StateError::JournalReplayFailed(format!(
                    "checkpoint append failed ({err}); rollback to offset {append_start} failed ({rollback_err})"
                )));
            }
            return Err(StateError::io_at(
                "append and fsync checkpoint journal frame",
                &self.path,
                err,
            ));
        }
        Ok(())
    }
}

fn checkpoint_companion_path(path: &Path, suffix: &str) -> PathBuf {
    let mut companion = path.as_os_str().to_os_string();
    companion.push(suffix);
    PathBuf::from(companion)
}

fn terminal_finalization_intent_path(path: &Path) -> PathBuf {
    checkpoint_companion_path(path, ".finalizing")
}

fn write_terminal_finalization_intent(
    path: &Path,
    intent: &TerminalFinalizationIntent,
    replace: bool,
) -> Result<(), StateError> {
    intent.validate_for_tape(intent.tape_uuid)?;
    let intent_path = terminal_finalization_intent_path(path);
    if !replace {
        match fs::symlink_metadata(&intent_path) {
            Ok(_) => {
                return Err(StateError::JournalReplayFailed(format!(
                    "checkpoint journal {} already has a pending terminal finalization intent",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(StateError::io_at(
                    "inspect terminal finalization intent",
                    &intent_path,
                    error,
                ));
            }
        }
    }

    let payload = serde_json::to_vec(intent).map_err(|error| {
        StateError::JournalReplayFailed(format!("encode terminal finalization intent: {error}"))
    })?;
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        StateError::JournalReplayFailed(
            "terminal finalization intent length does not fit u32".to_string(),
        )
    })?;
    if u64::from(payload_len) > MAX_FINALIZATION_INTENT_LEN {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal finalization intent length {payload_len} exceeds limit {MAX_FINALIZATION_INTENT_LEN}"
        )));
    }
    let frame_len = CHECKPOINT_FINALIZATION_INTENT_MAGIC
        .len()
        .checked_add(2)
        .and_then(|len| len.checked_add(4))
        .and_then(|len| len.checked_add(payload.len()))
        .and_then(|len| len.checked_add(8))
        .ok_or_else(|| {
            StateError::JournalReplayFailed(
                "terminal finalization intent frame length overflows usize".to_string(),
            )
        })?;
    let mut frame = Vec::with_capacity(frame_len);
    frame.extend_from_slice(CHECKPOINT_FINALIZATION_INTENT_MAGIC);
    frame.extend_from_slice(&CHECKPOINT_FINALIZATION_INTENT_VERSION.to_le_bytes());
    frame.extend_from_slice(&payload_len.to_le_bytes());
    frame.extend_from_slice(&payload);
    let crc = remanence_parity::crc64_xz(&frame);
    frame.extend_from_slice(&crc.to_le_bytes());

    let temporary_path = checkpoint_companion_path(path, ".finalizing.new");
    let mut temporary = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary_path)
        .map_err(|error| {
            StateError::io_at(
                "create temporary terminal finalization intent",
                &temporary_path,
                error,
            )
        })?;
    temporary
        .write_all(&frame)
        .and_then(|_| temporary.sync_all())
        .map_err(|error| {
            StateError::io_at(
                "write temporary terminal finalization intent",
                &temporary_path,
                error,
            )
        })?;
    fs::rename(&temporary_path, &intent_path).map_err(|error| {
        StateError::io_at("publish terminal finalization intent", &intent_path, error)
    })?;
    sync_companion_parent(&intent_path, "fsync terminal finalization intent directory")
}

fn read_terminal_finalization_intent(
    path: &Path,
    tape_uuid: [u8; 16],
) -> Result<Option<TerminalFinalizationIntent>, StateError> {
    let intent_path = terminal_finalization_intent_path(path);
    let mut file = match File::open(&intent_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(StateError::io_at(
                "open terminal finalization intent",
                &intent_path,
                error,
            ));
        }
    };
    let file_len = file
        .metadata()
        .map_err(|error| {
            StateError::io_at("stat terminal finalization intent", &intent_path, error)
        })?
        .len();
    let minimum_len = u64::try_from(CHECKPOINT_FINALIZATION_INTENT_MAGIC.len() + 2 + 4 + 8)
        .expect("finalization intent fixed framing length fits u64");
    if file_len < minimum_len {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal finalization intent {} is truncated at {file_len} bytes",
            intent_path.display()
        )));
    }
    let mut prefix = [0u8; 14];
    file.read_exact(&mut prefix).map_err(|error| {
        StateError::io_at(
            "read terminal finalization intent prefix",
            &intent_path,
            error,
        )
    })?;
    if &prefix[..8] != CHECKPOINT_FINALIZATION_INTENT_MAGIC {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal finalization intent {} has invalid magic",
            intent_path.display()
        )));
    }
    let version = u16::from_le_bytes(prefix[8..10].try_into().expect("fixed version slice"));
    if version != CHECKPOINT_FINALIZATION_INTENT_VERSION {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal finalization intent {} has unsupported version {version}",
            intent_path.display()
        )));
    }
    let payload_len = u32::from_le_bytes(
        prefix[10..14]
            .try_into()
            .expect("fixed payload-length slice"),
    );
    if u64::from(payload_len) > MAX_FINALIZATION_INTENT_LEN {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal finalization intent {} declares {payload_len} bytes above limit {MAX_FINALIZATION_INTENT_LEN}",
            intent_path.display()
        )));
    }
    let expected_len = minimum_len
        .checked_add(u64::from(payload_len))
        .ok_or_else(|| {
            StateError::JournalReplayFailed(
                "terminal finalization intent length overflows u64".to_string(),
            )
        })?;
    if file_len != expected_len {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal finalization intent {} length {file_len} differs from declared {expected_len}",
            intent_path.display()
        )));
    }
    let payload_len = usize::try_from(payload_len).map_err(|_| {
        StateError::JournalReplayFailed(
            "terminal finalization intent length does not fit usize".to_string(),
        )
    })?;
    let mut payload = vec![0u8; payload_len];
    let mut stored_crc = [0u8; 8];
    file.read_exact(&mut payload)
        .and_then(|_| file.read_exact(&mut stored_crc))
        .map_err(|error| {
            StateError::io_at("read terminal finalization intent", &intent_path, error)
        })?;
    let mut covered = Vec::with_capacity(prefix.len() + payload.len());
    covered.extend_from_slice(&prefix);
    covered.extend_from_slice(&payload);
    if remanence_parity::crc64_xz(&covered) != u64::from_le_bytes(stored_crc) {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal finalization intent {} failed CRC validation",
            intent_path.display()
        )));
    }
    let intent: TerminalFinalizationIntent = serde_json::from_slice(&payload).map_err(|error| {
        StateError::JournalReplayFailed(format!(
            "decode terminal finalization intent {}: {error}",
            intent_path.display()
        ))
    })?;
    intent.validate_for_tape(tape_uuid)?;
    Ok(Some(intent))
}

fn clear_terminal_finalization_intent(path: &Path) -> Result<(), StateError> {
    let intent_path = terminal_finalization_intent_path(path);
    fs::remove_file(&intent_path).map_err(|error| {
        StateError::io_at("clear terminal finalization intent", &intent_path, error)
    })?;
    sync_companion_parent(
        &intent_path,
        "fsync cleared terminal finalization intent directory",
    )
}

fn sync_companion_parent(path: &Path, action: &str) -> Result<(), StateError> {
    let parent = path.parent().ok_or_else(|| {
        StateError::JournalReplayFailed(format!(
            "checkpoint companion {} has no parent directory",
            path.display()
        ))
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| StateError::io_at(action, parent, error))
}

fn enforce_terminal_intent_for_replay(
    path: &Path,
    tape_uuid: [u8; 16],
    records: &[CheckpointJournalRecord],
    clear_completed: bool,
) -> Result<(), StateError> {
    let structured_pending = read_terminal_finalization_intent(path, tape_uuid)?;
    if let Some(intent) = &structured_pending {
        intent.validate_for_checkpoint_prefix(finalization_base_record(records))?;
    }
    if structured_pending.is_none() {
        return Ok(());
    }
    if records
        .last()
        .is_some_and(|record| record.sealed_after_write)
    {
        if let Some(intent) = structured_pending.as_ref() {
            let completion = records
                .last()
                .and_then(|record| record.terminal_finalization.as_ref())
                .ok_or_else(|| {
                    StateError::JournalReplayFailed(format!(
                        "checkpoint journal {} has sealed authority but does not preserve its pending structured terminal finalization",
                        path.display()
                    ))
                })?;
            if completion != intent {
                return Err(StateError::JournalReplayFailed(format!(
                    "checkpoint journal {} sealed completion differs from its pending structured terminal finalization",
                    path.display()
                )));
            }
        }
        if clear_completed {
            clear_terminal_finalization_intent(path)?;
        }
        return Ok(());
    }
    Err(StateError::JournalReplayFailed(format!(
        "checkpoint journal {} has a pending terminal finalization intent without terminal authority; physical-tail reconciliation is required before append",
        path.display()
    )))
}

fn finalization_base_record(
    records: &[CheckpointJournalRecord],
) -> Option<&CheckpointJournalRecord> {
    match records.last() {
        Some(record) if record.terminal_finalization.is_some() => {
            records.get(records.len().saturating_sub(2))
        }
        other => other,
    }
}

fn acquire_checkpoint_lock(
    path: &Path,
    operation: FlockArg,
    action: &str,
) -> Result<Flock<File>, StateError> {
    let lock_path = checkpoint_companion_path(path, ".lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|err| StateError::io_at("open checkpoint journal lock", &lock_path, err))?;
    Flock::lock(lock_file, operation).map_err(|(_file, errno)| {
        StateError::io_at(action, &lock_path, std::io::Error::from(errno))
    })
}

fn initialize_checkpoint_file(path: &Path, tape_uuid: [u8; 16]) -> Result<File, StateError> {
    let init_path = checkpoint_companion_path(path, ".init");
    let mut init = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(&init_path)
        .map_err(|err| {
            StateError::io_at(
                "create checkpoint journal initialization file",
                &init_path,
                err,
            )
        })?;
    write_checkpoint_header(&mut init, tape_uuid, &init_path)?;
    if path.exists() {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint journal {} appeared during locked initialization",
            path.display()
        )));
    }
    fs::rename(&init_path, path)
        .map_err(|err| StateError::io_at("publish checkpoint journal header", path, err))?;
    let parent = path.parent().ok_or_else(|| {
        StateError::JournalReplayFailed(
            "checkpoint journal path has no parent directory".to_string(),
        )
    })?;
    File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|err| StateError::io_at("fsync checkpoint journal directory", parent, err))?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|err| StateError::io_at("open initialized checkpoint journal", path, err))
}

fn replay_checkpoint_records(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
) -> Result<Vec<CheckpointJournalRecord>, StateError> {
    let scan = scan_checkpoint_records(file, tape_uuid, path)?;
    match scan.tail {
        CheckpointReplayTail::Clean => Ok(scan.records),
        CheckpointReplayTail::Torn => Err(torn_checkpoint_tail_error(path, scan.valid_end)),
    }
}

fn visit_checkpoint_records_bounded(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
    visitor: impl FnMut(&CheckpointJournalRecord) -> Result<(), StateError>,
) -> Result<BoundedCheckpointReplay, StateError> {
    let file_len = file
        .metadata()
        .map_err(|err| StateError::io_at("stat checkpoint journal", path, err))?
        .len();
    visit_checkpoint_records_bounded_to(file, tape_uuid, path, file_len, visitor)
}

fn visit_checkpoint_records_bounded_to(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
    file_len: u64,
    mut visitor: impl FnMut(&CheckpointJournalRecord) -> Result<(), StateError>,
) -> Result<BoundedCheckpointReplay, StateError> {
    read_checkpoint_header(file, tape_uuid, path)?;
    let mut previous = None;
    let mut metrics = CheckpointBoundedReplayMetrics {
        replay_passes: 1,
        ..CheckpointBoundedReplayMetrics::default()
    };
    loop {
        let record_start = file
            .stream_position()
            .map_err(|err| StateError::io_at("position checkpoint journal", path, err))?;
        if record_start == file_len {
            return Ok(BoundedCheckpointReplay {
                last: previous,
                metrics,
            });
        }
        let available = file_len.checked_sub(record_start).ok_or_else(|| {
            StateError::JournalReplayFailed(
                "checkpoint replay position exceeds file length".to_string(),
            )
        })?;
        if available < CHECKPOINT_RECORD_PREFIX_LEN {
            return Err(torn_checkpoint_tail_error(path, record_start));
        }
        let mut prefix = [0u8; 6];
        file.read_exact(&mut prefix)
            .map_err(|err| StateError::io_at("read checkpoint record prefix", path, err))?;
        let version = u16::from_le_bytes(prefix[..2].try_into().expect("slice length"));
        if version != CHECKPOINT_RECORD_VERSION {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record at offset {record_start} in {} has unsupported version {version}",
                path.display()
            )));
        }
        let payload_len = u64::from(u32::from_le_bytes(
            prefix[2..6].try_into().expect("slice length"),
        ));
        if payload_len > MAX_CHECKPOINT_RECORD_LEN {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record at offset {record_start} in {} declares {payload_len} bytes, limit {MAX_CHECKPOINT_RECORD_LEN}",
                path.display()
            )));
        }
        let frame_len = CHECKPOINT_RECORD_PREFIX_LEN
            .checked_add(payload_len)
            .and_then(|value| value.checked_add(8))
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "checkpoint record frame length overflows u64".to_string(),
                )
            })?;
        if frame_len > available {
            return Err(torn_checkpoint_tail_error(path, record_start));
        }
        let payload_len = usize::try_from(payload_len).map_err(|_| {
            StateError::JournalReplayFailed(
                "checkpoint record length does not fit usize".to_string(),
            )
        })?;
        let mut payload = vec![0u8; payload_len];
        file.read_exact(&mut payload)
            .map_err(|err| StateError::io_at("read checkpoint record payload", path, err))?;
        let mut crc = [0u8; 8];
        file.read_exact(&mut crc)
            .map_err(|err| StateError::io_at("read checkpoint record checksum", path, err))?;
        let mut crc_input =
            Vec::with_capacity(prefix.len().checked_add(payload.len()).ok_or_else(|| {
                StateError::JournalReplayFailed("checkpoint CRC input overflows usize".to_string())
            })?);
        crc_input.extend_from_slice(&prefix);
        crc_input.extend_from_slice(&payload);
        if remanence_parity::crc64_xz(&crc_input) != u64::from_le_bytes(crc) {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record at offset {record_start} in {} has a checksum mismatch",
                path.display()
            )));
        }
        let mut frame: CheckpointJournalFrame =
            serde_json::from_slice(&payload).map_err(|err| {
                StateError::JournalReplayFailed(format!(
                    "decode checkpoint journal frame at offset {record_start} in {}: {err}",
                    path.display()
                ))
            })?;
        if frame.records.is_empty() {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint journal frame at offset {record_start} in {} is empty",
                path.display()
            )));
        }
        metrics.frame_count = metrics.frame_count.checked_add(1).ok_or_else(|| {
            StateError::JournalReplayFailed("checkpoint frame count overflows u64".to_string())
        })?;
        metrics.peak_frame_payload_bytes =
            metrics
                .peak_frame_payload_bytes
                .max(u64::try_from(payload.len()).map_err(|_| {
                    StateError::JournalReplayFailed(
                        "checkpoint frame payload length exceeds u64".to_string(),
                    )
                })?);
        let prior_record_count = u64::from(previous.is_some());
        let frame_record_count = u64::try_from(frame.records.len()).map_err(|_| {
            StateError::JournalReplayFailed("checkpoint record count exceeds u64".to_string())
        })?;
        metrics.peak_live_record_count = metrics.peak_live_record_count.max(
            prior_record_count
                .checked_add(frame_record_count)
                .ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "checkpoint live record count overflows u64".to_string(),
                    )
                })?,
        );
        let prior_object_rows = previous.as_ref().map_or(Ok(0), |record| {
            u64::try_from(record.objects.len()).map_err(|_| {
                StateError::JournalReplayFailed(
                    "checkpoint Object-row count exceeds u64".to_string(),
                )
            })
        })?;
        let frame_object_rows = frame.records.iter().try_fold(0u64, |count, record| {
            count
                .checked_add(u64::try_from(record.objects.len()).map_err(|_| {
                    StateError::JournalReplayFailed(
                        "checkpoint Object-row count exceeds u64".to_string(),
                    )
                })?)
                .ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "checkpoint live Object-row count overflows u64".to_string(),
                    )
                })
        })?;
        metrics.peak_live_object_rows = metrics.peak_live_object_rows.max(
            prior_object_rows
                .checked_add(frame_object_rows)
                .ok_or_else(|| {
                    StateError::JournalReplayFailed(
                        "checkpoint live Object-row count overflows u64".to_string(),
                    )
                })?,
        );
        for index in 0..frame.records.len() {
            let prior = if index == 0 {
                previous.as_ref()
            } else {
                frame.records.get(index - 1)
            };
            let record = &frame.records[index];
            if record.tape_uuid != tape_uuid {
                return Err(StateError::JournalReplayFailed(format!(
                    "checkpoint record at offset {record_start} tape_uuid mismatch in {}",
                    path.display()
                )));
            }
            validate_next_record(prior, record)?;
            visitor(record)?;
        }
        previous = frame.records.pop();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckpointReplayTail {
    Clean,
    Torn,
}

#[derive(Debug)]
struct CheckpointReplayScan {
    records: Vec<CheckpointJournalRecord>,
    valid_end: u64,
    tail: CheckpointReplayTail,
}

fn torn_checkpoint_tail_error(path: &Path, valid_end: u64) -> StateError {
    StateError::JournalReplayFailed(format!(
        "checkpoint journal {} has a torn trailing frame after offset {valid_end}; explicit recovery is required before append",
        path.display()
    ))
}

fn write_checkpoint_header(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
) -> Result<(), StateError> {
    let mut header = Vec::with_capacity(
        usize::try_from(CHECKPOINT_JOURNAL_HEADER_LEN)
            .expect("checkpoint header length fits usize"),
    );
    header.extend_from_slice(CHECKPOINT_JOURNAL_MAGIC);
    header.extend_from_slice(&tape_uuid);
    let crc = remanence_parity::crc64_xz(&header);
    header.extend_from_slice(&crc.to_le_bytes());
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&header))
        .and_then(|_| file.sync_all())
        .map_err(|err| StateError::io_at("write checkpoint journal header", path, err))
}

fn read_checkpoint_header(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
) -> Result<(), StateError> {
    file.seek(SeekFrom::Start(0))
        .map_err(|err| StateError::io_at("seek checkpoint journal header", path, err))?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header).map_err(|err| {
        StateError::JournalReplayFailed(format!(
            "checkpoint journal {} has a missing or torn versioned header: {err}",
            path.display()
        ))
    })?;
    let mut crc = [0u8; 8];
    file.read_exact(&mut crc).map_err(|err| {
        StateError::JournalReplayFailed(format!(
            "checkpoint journal {} has a torn header checksum: {err}",
            path.display()
        ))
    })?;
    if &header[..8] != CHECKPOINT_JOURNAL_MAGIC {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint journal {} uses an unsupported legacy or future format",
            path.display()
        )));
    }
    if header[8..24] != tape_uuid {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint journal {} header tape_uuid mismatch",
            path.display()
        )));
    }
    if remanence_parity::crc64_xz(&header) != u64::from_le_bytes(crc) {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint journal {} header checksum mismatch",
            path.display()
        )));
    }
    Ok(())
}

fn scan_checkpoint_records(
    file: &mut File,
    tape_uuid: [u8; 16],
    path: &Path,
) -> Result<CheckpointReplayScan, StateError> {
    read_checkpoint_header(file, tape_uuid, path)?;
    let file_len = file
        .metadata()
        .map_err(|err| StateError::io_at("stat checkpoint journal", path, err))?
        .len();
    let mut records = Vec::new();
    let mut valid_end = CHECKPOINT_JOURNAL_HEADER_LEN;
    loop {
        let record_start = file
            .stream_position()
            .map_err(|err| StateError::io_at("position checkpoint journal", path, err))?;
        if record_start == file_len {
            return Ok(CheckpointReplayScan {
                records,
                valid_end,
                tail: CheckpointReplayTail::Clean,
            });
        }
        let available = file_len.saturating_sub(record_start);
        if available < CHECKPOINT_RECORD_PREFIX_LEN {
            return Ok(CheckpointReplayScan {
                records,
                valid_end,
                tail: CheckpointReplayTail::Torn,
            });
        }
        let mut prefix = [0u8; 6];
        file.read_exact(&mut prefix)
            .map_err(|err| StateError::io_at("read checkpoint record prefix", path, err))?;
        let version = u16::from_le_bytes(prefix[..2].try_into().expect("slice length"));
        if version != CHECKPOINT_RECORD_VERSION {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record at offset {record_start} in {} has unsupported version {version}",
                path.display()
            )));
        }
        let payload_len = u64::from(u32::from_le_bytes(
            prefix[2..6].try_into().expect("slice length"),
        ));
        if payload_len > MAX_CHECKPOINT_RECORD_LEN {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record at offset {record_start} in {} declares {payload_len} bytes, limit {MAX_CHECKPOINT_RECORD_LEN}",
                path.display()
            )));
        }
        let frame_tail_len = payload_len.checked_add(8).ok_or_else(|| {
            StateError::JournalReplayFailed("checkpoint record length overflows u64".to_string())
        })?;
        if available.saturating_sub(CHECKPOINT_RECORD_PREFIX_LEN) < frame_tail_len {
            return Ok(CheckpointReplayScan {
                records,
                valid_end,
                tail: CheckpointReplayTail::Torn,
            });
        }
        let payload_len = usize::try_from(payload_len).map_err(|_| {
            StateError::JournalReplayFailed(
                "checkpoint record length does not fit usize".to_string(),
            )
        })?;
        let mut payload = vec![0u8; payload_len];
        file.read_exact(&mut payload)
            .map_err(|err| StateError::io_at("read checkpoint record payload", path, err))?;
        let mut crc = [0u8; 8];
        file.read_exact(&mut crc)
            .map_err(|err| StateError::io_at("read checkpoint record checksum", path, err))?;
        let mut crc_input = Vec::with_capacity(prefix.len() + payload.len());
        crc_input.extend_from_slice(&prefix);
        crc_input.extend_from_slice(&payload);
        if remanence_parity::crc64_xz(&crc_input) != u64::from_le_bytes(crc) {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint record at offset {record_start} in {} has a checksum mismatch",
                path.display()
            )));
        }
        let frame: CheckpointJournalFrame = serde_json::from_slice(&payload).map_err(|err| {
            StateError::JournalReplayFailed(format!(
                "decode checkpoint journal frame at offset {record_start} in {}: {err}",
                path.display()
            ))
        })?;
        if frame.records.is_empty() {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint journal frame at offset {record_start} in {} is empty",
                path.display()
            )));
        }
        for record in frame.records {
            if record.tape_uuid != tape_uuid {
                return Err(StateError::JournalReplayFailed(format!(
                    "checkpoint record at offset {record_start} tape_uuid mismatch in {}",
                    path.display()
                )));
            }
            validate_next_record(records.last(), &record)?;
            records.push(record);
        }
        valid_end = file
            .stream_position()
            .map_err(|err| StateError::io_at("position checkpoint journal", path, err))?;
    }
}

/// Enumerate all per-tape checkpoint journal paths in a configured directory.
pub fn list_checkpoint_journals(dir: impl AsRef<Path>) -> Result<Vec<PathBuf>, StateError> {
    let dir = dir.as_ref();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for entry in
        fs::read_dir(dir).map_err(|err| StateError::io_at("list checkpoint journals", dir, err))?
    {
        let path = entry
            .map_err(|err| StateError::io_at("read checkpoint journal directory entry", dir, err))?
            .path();
        if path.extension().is_some_and(|ext| ext == "remcheckpoint") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Decode the tape UUID embedded in a checkpoint journal filename.
pub fn tape_uuid_from_checkpoint_path(path: &Path) -> Result<[u8; 16], StateError> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            StateError::JournalReplayFailed(format!(
                "checkpoint journal path has no UTF-8 filename: {}",
                path.display()
            ))
        })?;
    let uuid = filename
        .strip_suffix(CHECKPOINT_JOURNAL_SUFFIX)
        .ok_or_else(|| {
            StateError::JournalReplayFailed(format!(
                "checkpoint journal filename has wrong suffix: {filename}"
            ))
        })?;
    uuid::Uuid::parse_str(uuid)
        .map(|uuid| *uuid.as_bytes())
        .map_err(|err| {
            StateError::JournalReplayFailed(format!(
                "checkpoint journal filename has invalid tape UUID {uuid:?}: {err}"
            ))
        })
}

fn checkpoint_journal_path(dir: &Path, tape_uuid: [u8; 16]) -> PathBuf {
    dir.join(format!(
        "{}{}",
        uuid::Uuid::from_bytes(tape_uuid),
        CHECKPOINT_JOURNAL_SUFFIX
    ))
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct CheckpointBundleShapeError(String);

#[derive(Debug)]
pub(crate) struct ValidatedParityCheckpointLayout<'a> {
    pub(crate) first_tape_file: &'a remanence_parity::TapeFileEntry,
    pub(crate) last_tape_file: &'a remanence_parity::TapeFileEntry,
    pub(crate) starting_total_committed_ordinals: u64,
    pub(crate) highest_protected_ordinal: u64,
    pub(crate) total_committed_ordinals: u64,
}

fn barrier_bundle_shape_error(detail: impl Into<String>) -> CheckpointBundleShapeError {
    CheckpointBundleShapeError(detail.into())
}

/// Validate the complete current-wire parity layout carried by one checkpoint
/// record. Journal append/replay and SQLite projection both call this function
/// so they cannot accept different control-bundle shapes.
pub(crate) fn validate_parity_barrier_bundles(
    record: &CheckpointJournalRecord,
) -> Result<ValidatedParityCheckpointLayout<'_>, CheckpointBundleShapeError> {
    if record.scheme.is_none() {
        return Err(barrier_bundle_shape_error(
            "parity bundle validation requires a parity scheme",
        ));
    }
    if record.object_tape_file_bundles.len() != record.objects.len() {
        return Err(barrier_bundle_shape_error(format!(
            "parity checkpoint has {} object bundles for {} object projections",
            record.object_tape_file_bundles.len(),
            record.objects.len()
        )));
    }
    let mut first_tape_file = None;
    let mut prior_last_tape_file = None;
    let mut highest_protected_ordinal = None;
    let mut total_committed_ordinals = None;
    let mut starting_total_committed_ordinals = None;

    for (projection, bundle) in record.objects.iter().zip(&record.object_tape_file_bundles) {
        remanence_parity::validate_committed_bundle_shape(bundle).map_err(|err| {
            barrier_bundle_shape_error(format!("object {}: {err}", projection.object.object_id))
        })?;
        if bundle.kind != remanence_parity::CommittedBundleKind::Object {
            return Err(barrier_bundle_shape_error(format!(
                "object {} uses {:?} bundle kind",
                projection.object.object_id, bundle.kind
            )));
        }
        let object_entry = bundle.entries.first().ok_or_else(|| {
            barrier_bundle_shape_error(format!(
                "object {} bundle is empty",
                projection.object.object_id
            ))
        })?;
        validate_next_bundle_file(
            prior_last_tape_file,
            object_entry,
            "parity checkpoint object bundle",
        )?;
        first_tape_file.get_or_insert(object_entry);

        if object_entry.object_id.as_deref() != Some(projection.object.object_id.as_str())
            || object_entry.tape_file_number != projection.copy.tape_file_number
            || object_entry.block_count != projection.block_count
        {
            return Err(barrier_bundle_shape_error(format!(
                "object {} bundle entry does not match projection geometry",
                projection.object.object_id
            )));
        }
        if object_entry.block_count == 0 {
            return Err(barrier_bundle_shape_error(format!(
                "object {} has zero stored blocks",
                projection.object.object_id
            )));
        }
        let object_first_ordinal = object_entry.first_parity_data_ordinal.ok_or_else(|| {
            barrier_bundle_shape_error(format!(
                "object {} has no first parity data ordinal",
                projection.object.object_id
            ))
        })?;
        let running_total = match total_committed_ordinals {
            Some(total) => total,
            None => {
                starting_total_committed_ordinals = Some(object_first_ordinal);
                highest_protected_ordinal = Some(object_first_ordinal);
                object_first_ordinal
            }
        };
        if object_first_ordinal != running_total {
            return Err(barrier_bundle_shape_error(format!(
                "object {} starts at parity ordinal {}, expected {}",
                projection.object.object_id, object_first_ordinal, running_total
            )));
        }
        let next_total = running_total
            .checked_add(object_entry.block_count)
            .ok_or_else(|| barrier_bundle_shape_error("checkpoint object ordinals overflow u64"))?;
        if bundle.total_committed_ordinals != next_total
            || projection.total_committed_ordinals != next_total
        {
            return Err(barrier_bundle_shape_error(format!(
                "object {} ends at ordinal {next_total}, but bundle/projection report {}/{}",
                projection.object.object_id,
                bundle.total_committed_ordinals,
                projection.total_committed_ordinals
            )));
        }
        let next_highest = validate_sidecar_watermark_transition(
            highest_protected_ordinal.expect("set with first object total"),
            next_total,
            bundle,
        )?;
        if bundle.highest_protected_ordinal != next_highest {
            return Err(barrier_bundle_shape_error(format!(
                "object {} bundle reports W={}, expected {next_highest} from its sidecars",
                projection.object.object_id, bundle.highest_protected_ordinal
            )));
        }
        highest_protected_ordinal = Some(next_highest);
        total_committed_ordinals = Some(next_total);
        prior_last_tape_file = bundle.entries.last();
    }

    let first_tape_file = first_tape_file.ok_or_else(|| {
        barrier_bundle_shape_error("parity checkpoint must commit at least one object")
    })?;
    let total_committed_ordinals =
        total_committed_ordinals.expect("a validated parity checkpoint has at least one object");
    let mut final_highest = highest_protected_ordinal.expect("a validated parity checkpoint has W");
    if let Some(barrier_bundle) = record.barrier_bundle.as_ref() {
        remanence_parity::validate_committed_bundle_shape(barrier_bundle)
            .map_err(|err| barrier_bundle_shape_error(format!("checkpoint barrier: {err}")))?;
        if barrier_bundle.kind != remanence_parity::CommittedBundleKind::CheckpointSidecars {
            return Err(barrier_bundle_shape_error(format!(
                "checkpoint barrier uses {:?} bundle kind",
                barrier_bundle.kind
            )));
        }
        let barrier_first = barrier_bundle.entries.first().ok_or_else(|| {
            barrier_bundle_shape_error(
                "an exact-boundary parity checkpoint must omit its empty barrier bundle",
            )
        })?;
        validate_next_bundle_file(
            prior_last_tape_file,
            barrier_first,
            "parity checkpoint barrier bundle",
        )?;
        if barrier_bundle.total_committed_ordinals != total_committed_ordinals {
            return Err(barrier_bundle_shape_error(format!(
                "checkpoint barrier reports T={}, expected {total_committed_ordinals}",
                barrier_bundle.total_committed_ordinals
            )));
        }
        final_highest = validate_sidecar_watermark_transition(
            final_highest,
            total_committed_ordinals,
            barrier_bundle,
        )?;
        if barrier_bundle.highest_protected_ordinal != final_highest {
            return Err(barrier_bundle_shape_error(format!(
                "checkpoint barrier reports W={}, expected {final_highest} from its sidecars",
                barrier_bundle.highest_protected_ordinal
            )));
        }
        prior_last_tape_file = barrier_bundle.entries.last();
    }
    if final_highest != total_committed_ordinals {
        return Err(barrier_bundle_shape_error(format!(
            "checkpoint barrier left ordinals unprotected: W={final_highest}, T={total_committed_ordinals}"
        )));
    }
    let last_tape_file = prior_last_tape_file.expect("validated checkpoint has an Object row");
    let expected_next_tape_file = last_tape_file
        .tape_file_number
        .checked_add(1)
        .ok_or_else(|| barrier_bundle_shape_error("checkpoint next tape-file number overflows"))?;
    if record.next_tape_file_number != expected_next_tape_file {
        return Err(barrier_bundle_shape_error(format!(
            "checkpoint next tape-file number is {}, expected {expected_next_tape_file}",
            record.next_tape_file_number
        )));
    }

    Ok(ValidatedParityCheckpointLayout {
        first_tape_file,
        last_tape_file,
        starting_total_committed_ordinals: starting_total_committed_ordinals
            .expect("a validated parity checkpoint has a starting ordinal"),
        highest_protected_ordinal: final_highest,
        total_committed_ordinals,
    })
}

/// Require the external checkpoint record and Layer 3c sink journal to name
/// the same durable resume boundary before append positioning or media
/// modification occurs.
///
/// The two journals cannot be appended atomically. A crash may therefore
/// leave either authority one fsync ahead of the other. Resuming from the EOD
/// in one while seeding logical ordinals from the other could overwrite a
/// newer tape prefix, so disagreement is a fail-closed recovery condition.
pub fn validate_parity_resume_authority(
    records: &[CheckpointJournalRecord],
    committed: &remanence_parity::CommittedState,
    tape_uuid: [u8; 16],
    block_size: u32,
    scheme: &remanence_parity::ParityScheme,
) -> Result<(), StateError> {
    let mismatch = |detail: String| {
        StateError::JournalReplayFailed(format!("parity resume authority mismatch: {detail}"))
    };
    if !committed.orphaned_bundles.is_empty() {
        return Err(mismatch(format!(
            "sink journal exposes {} preserved bundle(s) beyond its last checkpoint marker; physical-tail reconciliation is required before append",
            committed.orphaned_bundles.len()
        )));
    }
    let record = match (records.last(), committed.entries.is_empty()) {
        (None, true) => return Ok(()),
        (None, false) => {
            return Err(mismatch(
                "sink journal has a committed prefix but checkpoint journal is empty".to_string(),
            ));
        }
        (Some(_), true) => {
            return Err(mismatch(
                "checkpoint journal is nonempty but sink journal has no committed prefix"
                    .to_string(),
            ));
        }
        (Some(record), false) => record,
    };
    let layout = validate_parity_barrier_bundles(record)
        .map_err(|err| mismatch(format!("checkpoint record is invalid: {err}")))?;
    if record.tape_uuid != tape_uuid {
        return Err(mismatch(format!(
            "checkpoint tape {} does not match selected tape {}",
            uuid::Uuid::from_bytes(record.tape_uuid),
            uuid::Uuid::from_bytes(tape_uuid)
        )));
    }
    if record.block_size != block_size {
        return Err(mismatch(format!(
            "checkpoint block size {} does not match sink journal block size {block_size}",
            record.block_size
        )));
    }
    if record.scheme.as_ref() != Some(scheme) {
        return Err(mismatch(
            "checkpoint parity scheme does not match the sink journal".to_string(),
        ));
    }
    if committed.highest_protected_ordinal != layout.highest_protected_ordinal
        || committed.total_committed_ordinals != layout.total_committed_ordinals
    {
        return Err(mismatch(format!(
            "checkpoint W/T ({}/{}) does not match sink journal W/T ({}/{})",
            layout.highest_protected_ordinal,
            layout.total_committed_ordinals,
            committed.highest_protected_ordinal,
            committed.total_committed_ordinals
        )));
    }

    let committed_object_count = committed
        .entries
        .iter()
        .filter(|entry| entry.kind == remanence_parity::TapeFileKind::Object)
        .count();
    let committed_object_count = u64::try_from(committed_object_count)
        .map_err(|_| mismatch("sink journal object count overflows u64".to_string()))?;
    if committed_object_count != record.committed_object_count {
        return Err(mismatch(format!(
            "checkpoint names {} committed objects but sink journal contains {committed_object_count}",
            record.committed_object_count
        )));
    }

    let bot_bootstrap = &committed.entries[0];
    if bot_bootstrap.tape_file_number != 0
        || bot_bootstrap.kind != remanence_parity::TapeFileKind::Bootstrap
        || bot_bootstrap.block_count != 1
    {
        return Err(mismatch(format!(
            "sink journal does not start with the one-block BOT Bootstrap: {bot_bootstrap:?}"
        )));
    }

    let expected_prefix = records
        .iter()
        .flat_map(|record| {
            record
                .object_tape_file_bundles
                .iter()
                .flat_map(|bundle| bundle.entries.iter())
                .chain(
                    record
                        .barrier_bundle
                        .iter()
                        .flat_map(|bundle| bundle.entries.iter()),
                )
        })
        .collect::<Vec<_>>();
    let expected_sink_entries = expected_prefix
        .len()
        .checked_add(1)
        .ok_or_else(|| mismatch("checkpoint prefix entry count overflows usize".to_string()))?;
    if committed.entries.len() != expected_sink_entries {
        return Err(mismatch(format!(
            "checkpoint history names {expected_sink_entries} tape-file entries including BOT, but sink journal contains {}",
            committed.entries.len()
        )));
    }
    for (offset, (actual, expected)) in committed.entries[1..]
        .iter()
        .zip(expected_prefix)
        .enumerate()
    {
        if !parity_resume_entries_match(actual, expected) {
            return Err(mismatch(format!(
                "checkpoint and sink prefixes differ at entry {}: checkpoint={expected:?}, sink={actual:?}",
                offset + 1
            )));
        }
    }

    let expected_eod_lba = layout
        .last_tape_file
        .physical_start_hint
        .ok_or_else(|| mismatch("checkpoint final row has no physical start hint".to_string()))?
        .checked_add(layout.last_tape_file.block_count)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(|| mismatch("checkpoint EOD calculation overflows".to_string()))?;
    if record.eod_partition != 0 || record.eod_lba != expected_eod_lba {
        return Err(mismatch(format!(
            "checkpoint barrier position is partition {} lba {}, expected partition 0 lba {expected_eod_lba} from its final structural row",
            record.eod_partition, record.eod_lba
        )));
    }
    Ok(())
}

fn parity_resume_entries_match(
    sink: &remanence_parity::TapeFileEntry,
    checkpoint: &remanence_parity::TapeFileEntry,
) -> bool {
    if sink.kind != remanence_parity::TapeFileKind::Object
        || checkpoint.kind != remanence_parity::TapeFileKind::Object
    {
        return sink == checkpoint;
    }

    let Some(checkpoint_object_id) = checkpoint.object_id.as_deref() else {
        return false;
    };
    if sink
        .object_id
        .as_deref()
        .is_some_and(|sink_object_id| sink_object_id != checkpoint_object_id)
    {
        return false;
    }
    let mut sink = sink.clone();
    let mut checkpoint = checkpoint.clone();
    sink.object_id = None;
    checkpoint.object_id = None;
    sink.object_recovery_row = None;
    checkpoint.object_recovery_row = None;
    sink == checkpoint
}

fn validate_next_bundle_file(
    prior_last: Option<&remanence_parity::TapeFileEntry>,
    next_first: &remanence_parity::TapeFileEntry,
    context: &str,
) -> Result<(), CheckpointBundleShapeError> {
    let Some(prior_last) = prior_last else {
        return Ok(());
    };
    let expected = prior_last
        .tape_file_number
        .checked_add(1)
        .ok_or_else(|| barrier_bundle_shape_error("checkpoint tape-file number overflows u64"))?;
    if next_first.tape_file_number != expected {
        return Err(barrier_bundle_shape_error(format!(
            "{context} starts at tape file {}, expected {expected}",
            next_first.tape_file_number
        )));
    }
    Ok(())
}

pub(crate) fn validate_sidecar_watermark_transition(
    mut highest_protected_ordinal: u64,
    total_committed_ordinals: u64,
    bundle: &remanence_parity::CommittedBundle,
) -> Result<u64, CheckpointBundleShapeError> {
    for sidecar in bundle
        .entries
        .iter()
        .filter(|entry| entry.kind == remanence_parity::TapeFileKind::ParitySidecar)
    {
        let start = sidecar.protected_ordinal_start.ok_or_else(|| {
            barrier_bundle_shape_error(format!(
                "ParitySidecar at tape file {} has no protected range start",
                sidecar.tape_file_number
            ))
        })?;
        let end = sidecar.protected_ordinal_end_exclusive.ok_or_else(|| {
            barrier_bundle_shape_error(format!(
                "ParitySidecar at tape file {} has no protected range end",
                sidecar.tape_file_number
            ))
        })?;
        if start != highest_protected_ordinal || end <= start || end > total_committed_ordinals {
            return Err(barrier_bundle_shape_error(format!(
                "ParitySidecar at tape file {} protects [{start}, {end}), expected a non-empty range starting at {highest_protected_ordinal} and ending no later than {total_committed_ordinals}",
                sidecar.tape_file_number
            )));
        }
        highest_protected_ordinal = end;
    }
    Ok(highest_protected_ordinal)
}

fn validate_next_record(
    previous: Option<&CheckpointJournalRecord>,
    record: &CheckpointJournalRecord,
) -> Result<(), StateError> {
    if previous.is_some_and(|prior| prior.sealed_after_write) {
        return Err(StateError::JournalReplayFailed(
            "checkpoint record follows a terminal sealed checkpoint".to_string(),
        ));
    }
    let parity_record = record.scheme.is_some();
    if let Some(scheme) = &record.scheme {
        scheme.validate().map_err(|err| {
            StateError::JournalReplayFailed(format!(
                "parity checkpoint record carries an invalid scheme: {err}"
            ))
        })?;
    }
    if previous.is_some_and(|prior| prior.scheme != record.scheme) {
        return Err(StateError::JournalReplayFailed(
            "checkpoint parity scheme changed within one tape journal".to_string(),
        ));
    }
    let expected_ordinal = match previous {
        Some(prior) => prior.ordinal.checked_add(1).ok_or_else(|| {
            StateError::JournalReplayFailed("checkpoint ordinal overflows u64".to_string())
        })?,
        None => 1,
    };
    if record.ordinal != expected_ordinal {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint ordinal {} is not expected next ordinal {expected_ordinal}",
            record.ordinal
        )));
    }
    let prior_count = previous.map_or(0, |prior| prior.committed_object_count);
    let appended = u64::try_from(record.objects.len()).map_err(|_| {
        StateError::JournalReplayFailed("checkpoint object count exceeds u64".to_string())
    })?;
    let expected_count = prior_count.checked_add(appended).ok_or_else(|| {
        StateError::JournalReplayFailed(
            "checkpoint committed object count overflows u64".to_string(),
        )
    })?;
    if record.committed_object_count != expected_count {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint committed count {} does not extend prior count {prior_count} by {appended}",
            record.committed_object_count
        )));
    }
    if record.eod_partition != 0 {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint EOD partition {} is unsupported",
            record.eod_partition
        )));
    }
    if record.block_size == 0 {
        return Err(StateError::JournalReplayFailed(
            "checkpoint block size must be non-zero".to_string(),
        ));
    }
    if record.sealed_after_write {
        return validate_terminal_checkpoint_record(previous, record);
    }
    if record.terminal_finalization.is_some() {
        return Err(StateError::JournalReplayFailed(
            "non-terminal checkpoint carries terminal finalization completion".to_string(),
        ));
    }
    if record.objects.is_empty() {
        return Err(StateError::JournalReplayFailed(
            "non-terminal checkpoint record must commit at least one object".to_string(),
        ));
    }
    if !parity_record
        && (!record.object_tape_file_bundles.is_empty() || record.barrier_bundle.is_some())
    {
        return Err(StateError::JournalReplayFailed(
            "parity-off checkpoint record carries parity bundle fields".to_string(),
        ));
    }
    let expected_first_file = match previous {
        Some(prior) => prior.next_tape_file_number,
        None => 1,
    };
    let parity_layout = if parity_record {
        let layout = validate_parity_barrier_bundles(record)
            .map_err(|err| StateError::JournalReplayFailed(err.to_string()))?;
        if layout.first_tape_file.tape_file_number != expected_first_file {
            return Err(StateError::JournalReplayFailed(format!(
                "parity checkpoint starts at tape file {}, expected {expected_first_file}",
                layout.first_tape_file.tape_file_number
            )));
        }
        let expected_starting_total = previous
            .and_then(|prior| prior.objects.last())
            .map_or(0, |projection| projection.total_committed_ordinals);
        if layout.starting_total_committed_ordinals != expected_starting_total {
            return Err(StateError::JournalReplayFailed(format!(
                "parity checkpoint starts at ordinal {}, expected {expected_starting_total}",
                layout.starting_total_committed_ordinals
            )));
        }
        Some(layout)
    } else {
        None
    };
    let mut expected_file = expected_first_file;
    for (index, projection) in record.objects.iter().enumerate() {
        if projection.block_size != record.block_size {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} block size {} differs from record block size {}",
                projection.object.object_id, projection.block_size, record.block_size
            )));
        }
        if projection.copy.tape_uuid != record.tape_uuid {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} copy is on a different tape",
                projection.object.object_id
            )));
        }
        let object_file = if parity_record {
            record.object_tape_file_bundles[index].entries[0].tape_file_number
        } else {
            let object_file = expected_file;
            expected_file = expected_file.checked_add(1).ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "checkpoint object tape-file number overflows u64".to_string(),
                )
            })?;
            object_file
        };
        if projection.copy.tape_file_number != object_file {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} uses tape file {}, expected {object_file}",
                projection.object.object_id, projection.copy.tape_file_number,
            )));
        }
        let row = &projection.object_recovery_row;
        if row.tape_file_number != projection.copy.tape_file_number
            || row.stored_block_count != projection.block_count
        {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} recovery row does not match its copy geometry",
                projection.object.object_id
            )));
        }
        if row.object_id != projection.object.object_id.as_bytes() {
            return Err(StateError::JournalReplayFailed(format!(
                "checkpoint object {} recovery row has a different object_id",
                projection.object.object_id
            )));
        }
    }
    if parity_layout.is_none() && record.next_tape_file_number != expected_file {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint next tape-file number is {}, expected {expected_file}",
            record.next_tape_file_number
        )));
    }
    if previous.is_some_and(|prior| prior.block_size != record.block_size) {
        return Err(StateError::JournalReplayFailed(
            "checkpoint block size changed within one tape journal".to_string(),
        ));
    }
    if parity_record {
        let layout = parity_layout.expect("parity record has validated layout");
        let expected_eod = layout
            .last_tape_file
            .physical_start_hint
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "parity checkpoint final structural row has no physical start".to_string(),
                )
            })?
            .checked_add(layout.last_tape_file.block_count)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| {
                StateError::JournalReplayFailed(
                    "parity checkpoint EOD calculation overflows u64".to_string(),
                )
            })?;
        if record.eod_lba != expected_eod
            || previous.is_some_and(|prior| record.eod_lba <= prior.eod_lba)
        {
            return Err(StateError::JournalReplayFailed(
                "parity checkpoint EOD does not match its final structural boundary".to_string(),
            ));
        }
        return Ok(());
    }
    let prefix_lba = previous.map_or(2, |prior| prior.eod_lba);
    let expected_eod = record
        .objects
        .iter()
        .try_fold(prefix_lba, |lba, projection| {
            lba.checked_add(projection.block_count)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| {
                    StateError::JournalReplayFailed("checkpoint EOD LBA overflows u64".to_string())
                })
        })?;
    if record.eod_lba != expected_eod {
        return Err(StateError::JournalReplayFailed(format!(
            "checkpoint EOD LBA {} does not match structural prefix {expected_eod}",
            record.eod_lba
        )));
    }
    Ok(())
}

fn validate_terminal_checkpoint_record(
    previous: Option<&CheckpointJournalRecord>,
    record: &CheckpointJournalRecord,
) -> Result<(), StateError> {
    if !record.objects.is_empty() || !record.object_tape_file_bundles.is_empty() {
        return Err(StateError::JournalReplayFailed(
            "terminal checkpoint record must not commit objects".to_string(),
        ));
    }
    let finalization = record.terminal_finalization.as_ref().ok_or_else(|| {
        StateError::JournalReplayFailed(
            "terminal checkpoint is missing structured finalization authority".to_string(),
        )
    })?;
    validate_structured_terminal_checkpoint_record(previous, record, finalization)
}

fn validate_structured_terminal_checkpoint_record(
    previous: Option<&CheckpointJournalRecord>,
    record: &CheckpointJournalRecord,
    finalization: &TerminalFinalizationIntent,
) -> Result<(), StateError> {
    finalization.validate_for_checkpoint_prefix(previous)?;
    if finalization.progress != TerminalFinalizationProgress::AfterReplicaC {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal finalization completion requires AfterReplicaC, found {:?}",
            finalization.progress
        )));
    }
    if finalization.recovery_required {
        return Err(StateError::JournalReplayFailed(
            "terminal finalization completion cannot retain recovery-required intent".to_string(),
        ));
    }
    if finalization.layout.partition != record.eod_partition
        || finalization.layout.block_size != record.block_size
        || finalization.layout.expected_eod_lba != record.eod_lba
    {
        return Err(StateError::JournalReplayFailed(
            "terminal finalization completion geometry does not match checkpoint authority"
                .to_string(),
        ));
    }
    let bundle = record.barrier_bundle.as_ref().ok_or_else(|| {
        StateError::JournalReplayFailed(
            "structured terminal checkpoint has no final component bundle".to_string(),
        )
    })?;
    remanence_parity::validate_committed_bundle_shape(bundle).map_err(|error| {
        StateError::JournalReplayFailed(format!(
            "structured terminal checkpoint has invalid final component bundle: {error}"
        ))
    })?;
    if bundle.kind != remanence_parity::CommittedBundleKind::TerminalComponent {
        return Err(StateError::JournalReplayFailed(format!(
            "structured terminal checkpoint uses {:?} bundle kind instead of TerminalComponent",
            bundle.kind
        )));
    }
    let [entry] = bundle.entries.as_slice() else {
        return Err(StateError::JournalReplayFailed(
            "structured terminal checkpoint must carry exactly final replica C".to_string(),
        ));
    };
    let replica_c = finalization.layout.components[4];
    if replica_c.kind != TerminalFinalizationComponentKind::TapeIndexReplica
        || replica_c.ordinal != 3
    {
        return Err(StateError::JournalReplayFailed(
            "terminal finalization layout does not end in replica C".to_string(),
        ));
    }
    let expected_next_tape_file_number =
        replica_c.tape_file_number.checked_add(1).ok_or_else(|| {
            StateError::JournalReplayFailed(
                "terminal finalization next tape-file number overflows u64".to_string(),
            )
        })?;
    if entry.kind != remanence_parity::TapeFileKind::TapeIndexReplica
        || entry.tape_file_number != replica_c.tape_file_number
        || entry.block_count != replica_c.record_count
        || entry.physical_start_hint != Some(replica_c.start_lba)
        || entry.canonical_metadata_hash != Some(finalization.edition_digest)
        || record.next_tape_file_number != expected_next_tape_file_number
    {
        return Err(StateError::JournalReplayFailed(
            "terminal finalization completion does not match final replica C bundle".to_string(),
        ));
    }
    if let Some(prefix) = &finalization.terminal_prefix {
        if bundle.highest_protected_ordinal != prefix.committed_bundle.highest_protected_ordinal
            || bundle.total_committed_ordinals != prefix.committed_bundle.total_committed_ordinals
        {
            return Err(StateError::JournalReplayFailed(format!(
                "terminal replica C W/T ({}/{}) does not preserve prefix authority ({}/{})",
                bundle.highest_protected_ordinal,
                bundle.total_committed_ordinals,
                prefix.committed_bundle.highest_protected_ordinal,
                prefix.committed_bundle.total_committed_ordinals
            )));
        }
        if previous.is_some_and(|prior| prior.block_size != record.block_size) {
            return Err(StateError::JournalReplayFailed(
                "terminal checkpoint block size changed within one tape journal".to_string(),
            ));
        }
        Ok(())
    } else {
        validate_terminal_scope_and_block_size(previous, record, bundle)
    }
}

fn validate_terminal_scope_and_block_size(
    previous: Option<&CheckpointJournalRecord>,
    record: &CheckpointJournalRecord,
    bundle: &remanence_parity::CommittedBundle,
) -> Result<(), StateError> {
    let (prior_highest, prior_total) = previous.map_or((0, 0), checkpoint_record_watermarks);
    if bundle.highest_protected_ordinal != prior_highest
        || bundle.total_committed_ordinals != prior_total
    {
        return Err(StateError::JournalReplayFailed(format!(
            "terminal checkpoint W/T ({}/{}) does not preserve prior authority ({prior_highest}/{prior_total})",
            bundle.highest_protected_ordinal, bundle.total_committed_ordinals
        )));
    }
    if previous.is_some_and(|prior| prior.block_size != record.block_size) {
        return Err(StateError::JournalReplayFailed(
            "terminal checkpoint block size changed within one tape journal".to_string(),
        ));
    }
    Ok(())
}

fn checkpoint_record_watermarks(record: &CheckpointJournalRecord) -> (u64, u64) {
    record.barrier_bundle.as_ref().map_or_else(
        || {
            record.object_tape_file_bundles.last().map_or_else(
                || {
                    record.objects.last().map_or((0, 0), |projection| {
                        (0, projection.total_committed_ordinals)
                    })
                },
                |bundle| {
                    (
                        bundle.highest_protected_ordinal,
                        bundle.total_committed_ordinals,
                    )
                },
            )
        },
        |bundle| {
            (
                bundle.highest_protected_ordinal,
                bundle.total_committed_ordinals,
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(tape_uuid: [u8; 16]) -> CheckpointJournalRecord {
        let object_uuid = uuid::Uuid::from_bytes([0x51; 16]);
        CheckpointJournalRecord {
            ordinal: 1,
            committed_object_count: 1,
            eod_partition: 0,
            eod_lba: 6,
            tape_uuid,
            batch_id: [0x42; 16],
            next_tape_file_number: 2,
            block_size: 256 * 1024,
            objects: vec![CheckpointObjectProjection {
                object: NativeObjectProjectionInput {
                    object_id: object_uuid.to_string(),
                    caller_object_id: Some("checkpoint-test".to_string()),
                    body_format: "rem-object-v1".to_string(),
                    logical_size_bytes: Some(1),
                    content_hash: Some(vec![0x11; 32]),
                    metadata_hash: Some(vec![0x22; 32]),
                    created_at_utc: Some("2026-07-21T00:00:00Z".to_string()),
                },
                files: Vec::new(),
                copy: NativeObjectCopyProjectionInput {
                    object_id: object_uuid.to_string(),
                    tape_uuid,
                    tape_file_number: 1,
                    first_body_lba: 0,
                    first_parity_data_ordinal: None,
                    protected_until_ordinal: None,
                    status: "committed".to_string(),
                    representation: "plaintext".to_string(),
                    recipient_epoch_ids: None,
                    metadata_frame_len: None,
                    plaintext_digest: Some(vec![0x33; 32]),
                    stored_digest: Some(vec![0x33; 32]),
                },
                block_size: 256 * 1024,
                block_count: 3,
                fresh_tape: true,
                total_committed_ordinals: 3,
                object_recovery_row: CheckpointObjectRecoveryRow {
                    tape_file_number: 1,
                    stored_block_count: 3,
                    object_id: object_uuid.to_string().into_bytes(),
                    representation: CheckpointObjectRecoveryRepresentation::Plaintext {
                        manifest_first_chunk_lba: 1,
                        manifest_size_bytes: 1,
                        manifest_chunk_count: 1,
                        manifest_sha256: [0x44; 32],
                    },
                },
            }],
            scheme: None,
            object_tape_file_bundles: Vec::new(),
            barrier_bundle: None,
            terminal_finalization: None,
            sealed_after_write: false,
        }
    }

    fn second_record(tape_uuid: [u8; 16]) -> CheckpointJournalRecord {
        let mut record = record(tape_uuid);
        let object_uuid = uuid::Uuid::from_bytes([0x52; 16]);
        record.ordinal = 2;
        record.committed_object_count = 2;
        record.eod_lba = 10;
        record.batch_id = [0x43; 16];
        record.next_tape_file_number = 3;
        record.objects[0].object.object_id = object_uuid.to_string();
        record.objects[0].object.caller_object_id = Some("checkpoint-test-2".to_string());
        record.objects[0].copy.object_id = object_uuid.to_string();
        record.objects[0].copy.tape_file_number = 2;
        record.objects[0].fresh_tape = false;
        record.objects[0].total_committed_ordinals = 6;
        record.objects[0].object_recovery_row.tape_file_number = 2;
        record.objects[0].object_recovery_row.object_id = object_uuid.to_string().into_bytes();
        record
    }

    #[test]
    fn checkpoint_carriers_preserve_tape_file_numbers_beyond_u32() {
        let tape_uuid = [0x7A; 16];
        let object_tape_file_number = u64::from(u32::MAX) + 17;
        let next_tape_file_number = object_tape_file_number + 1;
        let mut source = record(tape_uuid);
        source.objects[0].copy.tape_file_number = object_tape_file_number;
        source.objects[0].object_recovery_row.tape_file_number = object_tape_file_number;
        source.next_tape_file_number = next_tape_file_number;

        let mut encoded = Vec::new();
        ciborium::ser::into_writer(&source, &mut encoded).expect("encode checkpoint carrier");
        let decoded: CheckpointJournalRecord =
            ciborium::de::from_reader(encoded.as_slice()).expect("decode checkpoint carrier");
        assert_eq!(
            decoded.objects[0].copy.tape_file_number,
            object_tape_file_number
        );
        assert_eq!(
            decoded.objects[0].object_recovery_row.tape_file_number,
            object_tape_file_number
        );
        assert_eq!(decoded.next_tape_file_number, next_tape_file_number);
        assert_eq!(
            decoded.objects[0]
                .object_recovery_row
                .to_parity_row()
                .tape_file_number,
            object_tape_file_number
        );
    }

    fn finalization_intent(tape_uuid: [u8; 16]) -> TerminalFinalizationIntent {
        let layout = remanence_parity::TerminalTailLayout::new(0, 256 * 1024, 2, 6, 4, 4_096)
            .expect("terminal layout");
        TerminalFinalizationIntent {
            tape_uuid,
            trigger: TerminalFinalizationTrigger::OperatorCloseOut,
            manual: Some(ManualTerminalFinalizationIdentity {
                operation_id: [0x61; 16],
                operation_kind: "finalize_tape".to_string(),
                actor_fingerprint: "sha256:operator".to_string(),
                idempotency_key: [0x62; 16],
                request_fingerprint: [0x63; 32],
                assigned_pool_id: Some("slow-offsite".to_string()),
                expected_pool_id: Some("slow-offsite".to_string()),
                assignment_generation: 7,
                reason: "ship this partial copy offsite".to_string(),
            }),
            progress: TerminalFinalizationProgress::BeforeReplicaA,
            recovery_required: false,
            edition_id: [0x64; 16],
            edition_sequence: 1,
            edition_digest: [0x65; 32],
            writer_version: "remanence-test".to_string(),
            write_timestamp: "2026-08-09T00:00:00Z".to_string(),
            terminal_prefix: None,
            layout: TerminalFinalizationLayout::try_from(layout).expect("persisted layout"),
        }
    }

    fn completed_finalization_intent(tape_uuid: [u8; 16]) -> TerminalFinalizationIntent {
        let mut intent = finalization_intent(tape_uuid);
        intent.progress = TerminalFinalizationProgress::AfterReplicaC;
        intent
    }

    fn structured_terminal_record(
        prior: &CheckpointJournalRecord,
        finalization: TerminalFinalizationIntent,
    ) -> CheckpointJournalRecord {
        let replica_c = finalization.layout.components[4];
        CheckpointJournalRecord {
            ordinal: prior.ordinal + 1,
            committed_object_count: prior.committed_object_count,
            eod_partition: finalization.layout.partition,
            eod_lba: finalization.layout.expected_eod_lba,
            tape_uuid: prior.tape_uuid,
            batch_id: [0x66; 16],
            next_tape_file_number: replica_c
                .tape_file_number
                .checked_add(1)
                .expect("replica C tape-file number"),
            block_size: finalization.layout.block_size,
            objects: Vec::new(),
            scheme: prior.scheme.clone(),
            object_tape_file_bundles: Vec::new(),
            barrier_bundle: Some(remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::TerminalComponent,
                entries: vec![remanence_parity::TapeFileEntry {
                    tape_file_number: replica_c.tape_file_number,
                    kind: remanence_parity::TapeFileKind::TapeIndexReplica,
                    block_count: replica_c.record_count,
                    physical_start_hint: Some(replica_c.start_lba),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: Some(finalization.edition_digest),
                    object_recovery_row: None,
                }],
                highest_protected_ordinal: 0,
                total_committed_ordinals: prior
                    .objects
                    .last()
                    .map_or(0, |object| object.total_committed_ordinals),
            }),
            terminal_finalization: Some(finalization),
            sealed_after_write: true,
        }
    }

    fn advance_test_finalization_to_replica_c(
        lease: &mut FileCheckpointJournalLease,
        intent: &TerminalFinalizationIntent,
    ) {
        lease
            .begin_terminal_finalization(intent)
            .expect("publish structured finalization intent");
        for (expected, next) in [
            (
                TerminalFinalizationProgress::BeforeReplicaA,
                TerminalFinalizationProgress::AfterReplicaA,
            ),
            (
                TerminalFinalizationProgress::AfterReplicaA,
                TerminalFinalizationProgress::AfterSeparationAb,
            ),
            (
                TerminalFinalizationProgress::AfterSeparationAb,
                TerminalFinalizationProgress::AfterReplicaB,
            ),
            (
                TerminalFinalizationProgress::AfterReplicaB,
                TerminalFinalizationProgress::AfterSeparationBc,
            ),
            (
                TerminalFinalizationProgress::AfterSeparationBc,
                TerminalFinalizationProgress::AfterReplicaC,
            ),
        ] {
            lease
                .advance_terminal_finalization(expected, next)
                .expect("advance structured finalization component");
        }
    }

    fn parity_entry(
        tape_file_number: u64,
        kind: remanence_parity::TapeFileKind,
    ) -> remanence_parity::TapeFileEntry {
        remanence_parity::TapeFileEntry {
            tape_file_number,
            kind,
            block_count: 1,
            physical_start_hint: None,
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: None,
            object_recovery_row: None,
        }
    }

    fn partial_epoch_parity_record(tape_uuid: [u8; 16]) -> CheckpointJournalRecord {
        let mut record = record(tape_uuid);
        record.eod_lba = 12;
        record.next_tape_file_number = 3;
        record.scheme = Some(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("checkpoint-partial-epoch"),
            data_blocks_per_stripe: 8,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 1,
        });
        record.objects[0].fresh_tape = false;
        record.objects[0].copy.first_parity_data_ordinal = Some(0);
        record.objects[0].copy.protected_until_ordinal = Some(0);
        let mut object = parity_entry(1, remanence_parity::TapeFileKind::Object);
        object.block_count = 3;
        object.object_id = Some(record.objects[0].object.object_id.clone());
        object.first_parity_data_ordinal = Some(0);
        object.object_recovery_row = Some(record.objects[0].object_recovery_row.to_parity_row());
        record.object_tape_file_bundles = vec![remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::Object,
            entries: vec![object],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 3,
        }];
        let mut sidecar = parity_entry(2, remanence_parity::TapeFileKind::ParitySidecar);
        sidecar.epoch_id = Some(0);
        sidecar.protected_ordinal_start = Some(0);
        sidecar.protected_ordinal_end_exclusive = Some(3);
        sidecar.canonical_metadata_hash = Some([0x61; 32]);
        record.barrier_bundle = Some(remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::CheckpointSidecars,
            entries: vec![sidecar],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 3,
        });
        record
            .barrier_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries[0]
            .physical_start_hint = Some(10);
        record
    }

    fn second_partial_epoch_parity_record(
        prior: &CheckpointJournalRecord,
    ) -> CheckpointJournalRecord {
        let mut record = partial_epoch_parity_record(prior.tape_uuid);
        let object_uuid = uuid::Uuid::from_bytes([0x52; 16]);
        record.ordinal = 2;
        record.committed_object_count = 2;
        record.eod_lba = 22;
        record.batch_id = [0x43; 16];
        record.next_tape_file_number = 5;
        record.objects[0].object.object_id = object_uuid.to_string();
        record.objects[0].object.caller_object_id = Some("checkpoint-test-2".to_string());
        record.objects[0].copy.object_id = object_uuid.to_string();
        record.objects[0].copy.tape_file_number = 3;
        record.objects[0].copy.first_parity_data_ordinal = Some(3);
        record.objects[0].copy.protected_until_ordinal = Some(3);
        record.objects[0].total_committed_ordinals = 6;
        record.objects[0].object_recovery_row.tape_file_number = 3;
        record.objects[0].object_recovery_row.object_id = object_uuid.to_string().into_bytes();

        let mut object = parity_entry(3, remanence_parity::TapeFileKind::Object);
        object.block_count = 3;
        object.object_id = Some(object_uuid.to_string());
        object.first_parity_data_ordinal = Some(3);
        object.object_recovery_row = Some(record.objects[0].object_recovery_row.to_parity_row());
        record.object_tape_file_bundles = vec![remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::Object,
            entries: vec![object],
            highest_protected_ordinal: 3,
            total_committed_ordinals: 6,
        }];

        let mut sidecar = parity_entry(4, remanence_parity::TapeFileKind::ParitySidecar);
        sidecar.epoch_id = Some(1);
        sidecar.protected_ordinal_start = Some(3);
        sidecar.protected_ordinal_end_exclusive = Some(6);
        sidecar.canonical_metadata_hash = Some([0x62; 32]);
        sidecar.physical_start_hint = Some(20);
        record.barrier_bundle = Some(remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::CheckpointSidecars,
            entries: vec![sidecar],
            highest_protected_ordinal: 6,
            total_committed_ordinals: 6,
        });
        record
    }

    fn committed_state_for_parity_records(
        records: &[CheckpointJournalRecord],
    ) -> remanence_parity::CommittedState {
        let mut entries = vec![parity_entry(0, remanence_parity::TapeFileKind::Bootstrap)];
        for record in records {
            entries.extend(
                record
                    .object_tape_file_bundles
                    .iter()
                    .flat_map(|bundle| bundle.entries.iter().cloned()),
            );
            entries.extend(
                record
                    .barrier_bundle
                    .as_ref()
                    .expect("checkpoint bundle")
                    .entries
                    .iter()
                    .cloned(),
            );
        }
        for entry in &mut entries {
            if entry.kind == remanence_parity::TapeFileKind::Object {
                entry.object_id = None;
            }
        }
        let barrier_bundle = records
            .last()
            .expect("at least one checkpoint record")
            .barrier_bundle
            .as_ref()
            .expect("checkpoint bundle");
        remanence_parity::CommittedState {
            entries,
            highest_protected_ordinal: barrier_bundle.highest_protected_ordinal,
            total_committed_ordinals: barrier_bundle.total_committed_ordinals,
            orphaned_bundles: Vec::new(),
        }
    }

    fn terminal_prefix_plan_for_parity_record(
        record: &CheckpointJournalRecord,
    ) -> TerminalFinalizationPrefixPlan {
        let mut parity_map = parity_entry(3, remanence_parity::TapeFileKind::ParityMap);
        parity_map.block_count = 2;
        parity_map.physical_start_hint = Some(record.eod_lba);
        parity_map.canonical_metadata_hash = Some([0x70; 32]);
        TerminalFinalizationPrefixPlan {
            start_tape_file_number: 3,
            tail_start_tape_file_number: 4,
            start_lba: record.eod_lba,
            tail_start_lba: record.eod_lba + 3,
            parity_map_tape_file_number: Some(3),
            sidecar_directory_entries: vec![TerminalFinalizationSidecarDirectoryEntry {
                tape_file_number: 2,
                epoch_id: 0,
                protected_ordinal_start: 0,
                protected_ordinal_end_exclusive: 3,
                sidecar_total_block_count: 1,
                sidecar_header_block_count: 1,
                parity_shard_block_count: 1,
                canonical_metadata_hash: [0x61; 32],
                flags: remanence_parity::SIDECAR_DIRECTORY_FLAG_PRIMARY_KNOWN_GOOD
                    | remanence_parity::SIDECAR_DIRECTORY_FLAG_TAIL_KNOWN_GOOD,
            }],
            committed_bundle: remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::TerminalPrefix,
                entries: vec![parity_map],
                highest_protected_ordinal: 3,
                total_committed_ordinals: 3,
            },
        }
    }

    #[test]
    fn parity_resume_requires_checkpoint_and_sink_journal_to_name_same_prefix() {
        let tape_uuid = [0x28; 16];
        let record = partial_epoch_parity_record(tape_uuid);
        let scheme = record.scheme.as_ref().expect("parity scheme");
        let committed = committed_state_for_parity_records(std::slice::from_ref(&record));

        validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &committed,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect("matching durable authorities permit resume");

        let mut wrong_identity = committed.clone();
        wrong_identity
            .entries
            .iter_mut()
            .find(|entry| entry.kind == remanence_parity::TapeFileKind::Object)
            .and_then(|entry| entry.object_recovery_row.as_mut())
            .expect("sink object row")
            .object_id = Some(b"different-object".to_vec());
        validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &wrong_identity,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect("checkpoint recovery rows, not sink enrichment, own Object identity");

        let mut wrong_inline_identity = committed.clone();
        wrong_inline_identity
            .entries
            .iter_mut()
            .find(|entry| entry.kind == remanence_parity::TapeFileKind::Object)
            .expect("sink object entry")
            .object_id = Some("different-object".to_string());
        validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &wrong_inline_identity,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("a conflicting inline sink object identity must fail closed");

        let mut sink_ahead = committed.clone();
        let mut newer_bootstrap = parity_entry(3, remanence_parity::TapeFileKind::Bootstrap);
        newer_bootstrap.physical_start_hint = Some(record.eod_lba);
        sink_ahead.entries.push(newer_bootstrap);
        let error = validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &sink_ahead,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("a sink journal one checkpoint ahead must fail closed");
        assert!(error
            .to_string()
            .contains("parity resume authority mismatch"));

        let mut stale_eod = record.clone();
        stale_eod.eod_lba -= 1;
        let error = validate_parity_resume_authority(
            std::slice::from_ref(&stale_eod),
            &committed,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("a stale physical checkpoint position must fail closed");
        assert!(error.to_string().contains("expected partition 0 lba 12"));

        let empty = remanence_parity::CommittedState {
            entries: Vec::new(),
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
            orphaned_bundles: Vec::new(),
        };
        validate_parity_resume_authority(&[], &empty, tape_uuid, record.block_size, scheme)
            .expect("two empty authorities describe fresh media");
        let error = validate_parity_resume_authority(
            std::slice::from_ref(&record),
            &empty,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("checkpoint-only authority must fail closed");
        assert!(error
            .to_string()
            .contains("sink journal has no committed prefix"));
        let error =
            validate_parity_resume_authority(&[], &committed, tape_uuid, record.block_size, scheme)
                .expect_err("sink-only authority must fail closed");
        assert!(error.to_string().contains("checkpoint journal is empty"));

        let second = second_partial_epoch_parity_record(&record);
        let records = vec![record.clone(), second];
        let committed = committed_state_for_parity_records(&records);
        validate_parity_resume_authority(
            &records,
            &committed,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect("the complete two-checkpoint prefix permits resume");

        let mut changed_old_prefix = committed;
        changed_old_prefix.entries[1].physical_start_hint = Some(99);
        validate_parity_resume_authority(
            &records,
            &changed_old_prefix,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect_err("a mismatch in an older checkpoint prefix must fail closed");
    }

    #[test]
    fn terminal_prefix_replay_extends_ordinary_parity_authority_exactly() {
        let tape_uuid = [0x79; 16];
        let record = partial_epoch_parity_record(tape_uuid);
        let records = vec![record.clone()];
        let base = committed_state_for_parity_records(&records);
        let prefix = terminal_prefix_plan_for_parity_record(&record);
        prefix.validate().expect("validate persisted prefix plan");
        let parity_plan = remanence_parity::TerminalPrefixPlan::try_from(&prefix)
            .expect("recover parity prefix plan");
        assert_eq!(TerminalFinalizationPrefixPlan::from(&parity_plan), prefix);

        let mut projected = base.clone();
        projected
            .entries
            .extend(prefix.committed_bundle.entries.iter().cloned());
        projected.highest_protected_ordinal = prefix.committed_bundle.highest_protected_ordinal;
        projected.total_committed_ordinals = prefix.committed_bundle.total_committed_ordinals;

        let ordinary = CheckpointTerminalIndexRecordSource::new(&records, Some(&projected))
            .expect_err("ordinary resume must not accept post-prefix authority");
        assert!(
            ordinary
                .to_string()
                .contains("parity resume authority mismatch"),
            "{ordinary}"
        );
        let source = CheckpointTerminalIndexRecordSource::new_after_terminal_prefix(
            &records, &base, &projected, &prefix,
        )
        .expect("replay exact persisted terminal prefix");
        assert_eq!(source.summary().scope.covered_prefix_tape_file_count, 4);
        assert_eq!(source.summary().scope.total_data_ordinals, 3);
        assert_eq!(source.summary().scope.highest_protected_ordinal, 3);

        let mut extra = projected.clone();
        extra.entries.push(parity_entry(
            4,
            remanence_parity::TapeFileKind::TapeIndexReplica,
        ));
        let error = CheckpointTerminalIndexRecordSource::new_after_terminal_prefix(
            &records, &base, &extra, &prefix,
        )
        .expect_err("unplanned post-prefix entry must fail closed");
        assert!(error.to_string().contains("exactly"), "{error}");
    }

    #[test]
    fn terminal_intent_validates_prefix_coordinates_and_digest_diagnostics() {
        let tape_uuid = [0x7B; 16];
        let record = partial_epoch_parity_record(tape_uuid);
        let prefix = terminal_prefix_plan_for_parity_record(&record);
        let layout = remanence_parity::TerminalTailLayout::new(
            0,
            record.block_size,
            prefix.tail_start_tape_file_number,
            prefix.tail_start_lba,
            4,
            4_096,
        )
        .expect("terminal layout after prefix");
        let mut intent = finalization_intent(tape_uuid);
        intent.layout = TerminalFinalizationLayout::try_from(layout).expect("persist layout");
        intent.terminal_prefix = Some(prefix);
        intent
            .validate_for_checkpoint_prefix(Some(&record))
            .expect("validate reconstructible parity intent");

        intent.writer_version.push('\n');
        let writer = intent
            .validate_for_checkpoint_prefix(Some(&record))
            .expect_err("non-printable writer version must fail closed");
        assert!(writer.to_string().contains("writer_version"), "{writer}");
        intent.writer_version = "remanence-test".to_string();
        intent.write_timestamp = "2026-08-09".to_string();
        let timestamp = intent
            .validate_for_checkpoint_prefix(Some(&record))
            .expect_err("non-RFC3339 write timestamp must fail closed");
        assert!(
            timestamp.to_string().contains("write_timestamp"),
            "{timestamp}"
        );
    }

    #[test]
    fn fsynced_partial_epoch_parity_checkpoint_round_trips() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x25; 16];
        let record = partial_epoch_parity_record(tape_uuid);
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");

        journal.append(&record).expect("append sidecar checkpoint");
        assert_eq!(
            journal.replay().expect("replay parity checkpoint"),
            vec![record]
        );
    }

    #[test]
    fn exclusive_checkpoint_lease_serializes_replay_through_append() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x24; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let mut lease = journal
            .acquire_exclusive()
            .expect("acquire write-session lease");
        assert!(lease.replay().expect("replay under lease").is_empty());

        journal
            .replay()
            .expect_err("shared replay must not overlap a retained write lease");
        journal
            .append(&record(tape_uuid))
            .expect_err("a second writer must fail before deriving the same prefix");

        lease
            .append(&record(tape_uuid))
            .expect("lease owner appends checkpoint");
        drop(lease);
        assert_eq!(
            journal.replay().expect("replay after lease release"),
            vec![record(tape_uuid)]
        );
    }

    #[test]
    fn first_use_contention_never_publishes_a_partial_checkpoint_header() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x23; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let creator_lock = acquire_checkpoint_lock(
            journal.path(),
            FlockArg::LockExclusiveNonblock,
            "hold simulated creator lock",
        )
        .expect("hold stable creation lock");

        journal
            .acquire_exclusive()
            .expect_err("a competing first writer must lose before creating authority");
        assert!(
            !journal.path().exists(),
            "first-use contention must not publish an empty final journal"
        );
        drop(creator_lock);

        let init_path = checkpoint_companion_path(journal.path(), ".init");
        std::fs::write(&init_path, b"simulated-crash-partial-header")
            .expect("seed unpublished partial initialization file");
        let mut lease = journal
            .acquire_exclusive()
            .expect("retry atomically publishes a complete header");
        assert!(lease.replay().expect("replay published header").is_empty());
        assert_eq!(
            std::fs::metadata(journal.path())
                .expect("stat published journal")
                .len(),
            CHECKPOINT_JOURNAL_HEADER_LEN
        );
        assert!(
            !init_path.exists(),
            "atomic rename consumes the initialization file"
        );
    }

    #[test]
    fn parity_checkpoint_validator_accepts_sidecars_or_an_exact_boundary() {
        let tape_uuid = [0x26; 16];
        let sidecar_checkpoint = partial_epoch_parity_record(tape_uuid);

        let mut exact_boundary = sidecar_checkpoint.clone();
        let sidecar = exact_boundary
            .barrier_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries
            .remove(0);
        exact_boundary.object_tape_file_bundles[0]
            .entries
            .push(sidecar);
        exact_boundary.object_tape_file_bundles[0].highest_protected_ordinal = 3;
        exact_boundary.barrier_bundle = None;

        for record in [sidecar_checkpoint, exact_boundary] {
            validate_parity_barrier_bundles(&record)
                .unwrap_or_else(|err| panic!("valid checkpoint boundary rejected: {err}"));
        }
    }

    #[test]
    fn parity_checkpoint_validator_rejects_unprotected_or_misnumbered_barriers() {
        let tape_uuid = [0x27; 16];
        let mut unprotected = partial_epoch_parity_record(tape_uuid);
        unprotected.barrier_bundle = None;
        unprotected.next_tape_file_number = 2;
        assert!(validate_parity_barrier_bundles(&unprotected)
            .expect_err("checkpoint must close the open epoch")
            .to_string()
            .contains("left ordinals unprotected"));

        let mut misnumbered = partial_epoch_parity_record(tape_uuid);
        misnumbered.next_tape_file_number = 9;
        assert!(validate_parity_barrier_bundles(&misnumbered)
            .expect_err("record must identify the first free tape file")
            .to_string()
            .contains("expected 3"));

        let mut discontinuous_sidecar = partial_epoch_parity_record(tape_uuid);
        discontinuous_sidecar
            .barrier_bundle
            .as_mut()
            .expect("checkpoint bundle")
            .entries[0]
            .protected_ordinal_start = Some(1);
        assert!(validate_parity_barrier_bundles(&discontinuous_sidecar)
            .expect_err("sidecar range must start at the prior watermark")
            .to_string()
            .contains("starting at 0"));
    }

    #[test]
    fn torn_final_frame_fails_closed_and_is_not_repaired_by_append() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x11; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("append checkpoint");
        let mut file = OpenOptions::new()
            .append(true)
            .open(journal.path())
            .expect("open torn tail");
        file.write_all(&CHECKPOINT_RECORD_VERSION.to_le_bytes()[..1])
            .expect("write torn checkpoint tail");
        file.sync_all().expect("sync torn checkpoint tail");
        let torn_len = file.metadata().expect("stat torn journal").len();
        drop(file);

        let replay_err = journal
            .replay()
            .expect_err("torn checkpoint authority must fail closed");
        assert!(
            replay_err.to_string().contains("torn trailing frame"),
            "{replay_err}"
        );
        let append_err = journal
            .append(&second_record(tape_uuid))
            .expect_err("append must not erase a torn checkpoint tail");
        assert!(
            append_err.to_string().contains("explicit recovery"),
            "{append_err}"
        );
        assert_eq!(
            std::fs::metadata(journal.path())
                .expect("stat preserved torn journal")
                .len(),
            torn_len,
            "failed append must preserve torn evidence"
        );
    }

    #[test]
    fn headerless_checkpoint_tail_fails_closed_as_legacy_or_incomplete() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x31; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(journal.path())
            .expect("open partial checkpoint record");
        file.write_all(b"{\"ordinal\":1")
            .expect("write incomplete checkpoint record");

        let err = journal
            .replay()
            .expect_err("headerless checkpoint bytes must fail closed");
        assert!(err.to_string().contains("versioned header"), "{err}");
    }

    #[test]
    fn checksum_damage_fails_closed_before_any_later_record() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x32; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("append first checkpoint");
        journal
            .append(&second_record(tape_uuid))
            .expect("append second checkpoint");

        let damage_offset = CHECKPOINT_JOURNAL_HEADER_LEN + CHECKPOINT_RECORD_PREFIX_LEN + 8;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(journal.path())
            .expect("open checkpoint for damage");
        file.seek(SeekFrom::Start(damage_offset))
            .expect("seek damaged payload byte");
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte).expect("read payload byte");
        byte[0] ^= 0x80;
        file.seek(SeekFrom::Start(damage_offset))
            .expect("reseek damaged payload byte");
        file.write_all(&byte).expect("damage payload byte");
        file.sync_all().expect("sync checkpoint damage");

        let err = journal
            .replay()
            .expect_err("checksum damage must fail closed");
        assert!(err.to_string().contains("checksum mismatch"), "{err}");
        let append_err = journal
            .append(&second_record(tape_uuid))
            .expect_err("damage must fence later append");
        assert!(
            append_err.to_string().contains("checksum mismatch"),
            "{append_err}"
        );
    }

    #[test]
    fn hostile_declared_record_length_rejects_before_allocation() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x33; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("create versioned journal");

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(journal.path())
            .expect("open checkpoint for hostile length");
        file.set_len(CHECKPOINT_JOURNAL_HEADER_LEN)
            .expect("truncate to header");
        file.seek(SeekFrom::End(0)).expect("seek after header");
        file.write_all(&CHECKPOINT_RECORD_VERSION.to_le_bytes())
            .expect("write record version");
        let hostile_len =
            u32::try_from(MAX_CHECKPOINT_RECORD_LEN + 1).expect("configured replay limit fits u32");
        file.write_all(&hostile_len.to_le_bytes())
            .expect("write hostile record length");
        file.sync_all().expect("sync hostile length");

        let err = journal
            .replay()
            .expect_err("hostile record length must reject");
        assert!(err.to_string().contains("declares"), "{err}");
        assert!(err.to_string().contains("limit"), "{err}");
    }

    #[test]
    fn append_rejects_non_monotonic_count() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x22; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("append checkpoint");
        let mut invalid = record(tape_uuid);
        invalid.ordinal = 2;
        invalid.committed_object_count = 9;
        let err = journal
            .append(&invalid)
            .expect_err("invalid count must reject");
        assert!(err.to_string().contains("committed count"), "{err}");
    }

    #[test]
    fn append_rejects_a_checkpoint_after_terminal_seal_authority() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x25; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let prior = record(tape_uuid);
        journal
            .append(&prior)
            .expect("append ordinary checkpoint authority");
        let terminal = structured_terminal_record(&prior, completed_finalization_intent(tape_uuid));
        let err = journal
            .append(&terminal)
            .expect_err("terminal authority must require a structured finalization intent");
        assert!(
            err.to_string()
                .contains("structured terminal finalization intent"),
            "{err}"
        );
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        advance_test_finalization_to_replica_c(&mut lease, &finalization_intent(tape_uuid));
        lease
            .append_terminal_finalization(std::slice::from_ref(&terminal))
            .expect("append terminal checkpoint authority");
        drop(lease);

        let err = journal
            .append(&second_record(tape_uuid))
            .expect_err("a terminal checkpoint must permanently close the journal");
        assert!(
            err.to_string().contains("terminal sealed checkpoint"),
            "{err}"
        );
    }

    #[test]
    fn structured_finalization_intent_is_idempotent_monotonic_and_append_fencing() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x71; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("append ordinary checkpoint");
        let intent = finalization_intent(tape_uuid);

        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        assert_eq!(
            lease
                .begin_terminal_finalization(&intent)
                .expect("publish finalization intent"),
            intent
        );
        assert_eq!(
            lease
                .begin_terminal_finalization(&intent)
                .expect("identical request joins durable intent"),
            intent
        );
        let mut conflicting = intent.clone();
        conflicting.manual.as_mut().expect("manual identity").reason =
            "different exact reason".to_string();
        assert!(matches!(
            lease.begin_terminal_finalization(&conflicting),
            Err(StateError::IdempotencyConflict(_))
        ));
        drop(lease);

        let error = journal
            .acquire_exclusive()
            .expect_err("ordinary Object admission remains fenced");
        assert!(
            error.to_string().contains("terminal finalization"),
            "{error}"
        );

        let mut recovery = journal
            .acquire_exclusive_for_terminal_recovery()
            .expect("recover structured finalization");
        let recovery_authority = recovery
            .replay_for_terminal_recovery()
            .expect("replay prefix without clearing pending fence");
        assert_eq!(recovery_authority.records, vec![record(tape_uuid)]);
        assert_eq!(recovery_authority.finalization_intent, Some(intent.clone()));
        let classified = recovery
            .mark_terminal_recovery_required()
            .expect("persist recovery-required classification");
        assert!(classified.recovery_required);
        assert_eq!(
            recovery
                .mark_terminal_recovery_required()
                .expect("repeat recovery-required classification"),
            classified
        );
        let transitions = [
            (
                TerminalFinalizationProgress::BeforeReplicaA,
                TerminalFinalizationProgress::AfterReplicaA,
            ),
            (
                TerminalFinalizationProgress::AfterReplicaA,
                TerminalFinalizationProgress::AfterSeparationAb,
            ),
            (
                TerminalFinalizationProgress::AfterSeparationAb,
                TerminalFinalizationProgress::AfterReplicaB,
            ),
            (
                TerminalFinalizationProgress::AfterReplicaB,
                TerminalFinalizationProgress::AfterSeparationBc,
            ),
            (
                TerminalFinalizationProgress::AfterSeparationBc,
                TerminalFinalizationProgress::AfterReplicaC,
            ),
        ];
        for (expected, next) in transitions {
            let advanced = recovery
                .advance_terminal_finalization(expected, next)
                .expect("advance one proved component");
            assert_eq!(advanced.progress, next);
            assert!(!advanced.recovery_required);
            assert_eq!(
                advanced.progress.completed_replicas(),
                next.completed_replicas()
            );
            assert_eq!(
                recovery
                    .advance_terminal_finalization(expected, next)
                    .expect("repeated progress publication is idempotent")
                    .progress,
                next
            );
        }
        assert!(recovery
            .advance_terminal_finalization(
                TerminalFinalizationProgress::AfterReplicaC,
                TerminalFinalizationProgress::BeforeReplicaA,
            )
            .is_err());
        recovery
            .mark_terminal_recovery_required()
            .expect("classify completed tail before host-only recovery");
        let cleared = recovery
            .clear_terminal_recovery_required_after_replica_c()
            .expect("clear recovery after complete tail");
        assert_eq!(
            cleared.progress,
            TerminalFinalizationProgress::AfterReplicaC
        );
        assert!(!cleared.recovery_required);
    }

    #[test]
    fn structured_completion_survives_intent_cleanup_and_checkpoint_only_replay() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x74; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let prior = record(tape_uuid);
        journal.append(&prior).expect("append ordinary authority");
        let initial = finalization_intent(tape_uuid);
        let completed = completed_finalization_intent(tape_uuid);

        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        lease
            .begin_terminal_finalization(&initial)
            .expect("publish structured intent");
        for (expected, next) in [
            (
                TerminalFinalizationProgress::BeforeReplicaA,
                TerminalFinalizationProgress::AfterReplicaA,
            ),
            (
                TerminalFinalizationProgress::AfterReplicaA,
                TerminalFinalizationProgress::AfterSeparationAb,
            ),
            (
                TerminalFinalizationProgress::AfterSeparationAb,
                TerminalFinalizationProgress::AfterReplicaB,
            ),
            (
                TerminalFinalizationProgress::AfterReplicaB,
                TerminalFinalizationProgress::AfterSeparationBc,
            ),
            (
                TerminalFinalizationProgress::AfterSeparationBc,
                TerminalFinalizationProgress::AfterReplicaC,
            ),
        ] {
            lease
                .advance_terminal_finalization(expected, next)
                .expect("advance proved terminal component");
        }
        let terminal = structured_terminal_record(&prior, completed.clone());
        lease
            .append_terminal_finalization(std::slice::from_ref(&terminal))
            .expect("append completion and clear transient intent");
        drop(lease);

        assert!(!terminal_finalization_intent_path(journal.path()).exists());
        let replayed = FileCheckpointJournal::open(dir.path(), tape_uuid)
            .expect("reopen checkpoint-only authority")
            .replay()
            .expect("replay completed terminal authority");
        assert_eq!(replayed, vec![prior, terminal]);
        let recovered = replayed
            .last()
            .and_then(|record| record.terminal_finalization.as_ref())
            .expect("terminal completion remains in checkpoint authority");
        assert_eq!(recovered, &completed);
        assert_eq!(
            recovered.progress,
            TerminalFinalizationProgress::AfterReplicaC
        );
        assert_eq!(recovered.progress.completed_replicas(), 3);
        assert_eq!(recovered.edition_digest, [0x65; 32]);
        assert_eq!(
            recovered.layout.layout_digest,
            completed.layout.layout_digest
        );
        assert!(replayed.last().expect("terminal record").sealed_after_write);
    }

    #[test]
    fn sealed_fsync_interruption_is_cleared_only_for_the_exact_completion() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x75; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let prior = record(tape_uuid);
        let completed = completed_finalization_intent(tape_uuid);
        let terminal = structured_terminal_record(&prior, completed.clone());
        journal.append(&prior).expect("append ordinary authority");
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        write_terminal_finalization_intent(journal.path(), &completed, false)
            .expect("publish completed intent");
        let interruption = lease
            .append_terminal_finalization_with_after_fsync(std::slice::from_ref(&terminal), || {
                Err(StateError::JournalReplayFailed(
                    "simulated interruption after sealed checkpoint fsync".to_string(),
                ))
            })
            .expect_err("interrupt before intent cleanup");
        assert!(interruption.to_string().contains("simulated interruption"));
        drop(lease);

        assert!(terminal_finalization_intent_path(journal.path()).exists());
        let mut recovery = journal
            .acquire_exclusive_for_terminal_recovery()
            .expect("acquire terminal recovery owner");
        let recovered = recovery
            .replay_for_terminal_recovery()
            .expect("matching completion safely clears stale intent");
        assert_eq!(recovered.records, vec![prior.clone(), terminal.clone()]);
        assert_eq!(recovered.finalization_intent, None);
        drop(recovery);
        assert!(!terminal_finalization_intent_path(journal.path()).exists());
        assert_eq!(
            journal.replay().expect("completed replay is idempotent"),
            vec![prior, terminal]
        );

        let mut mismatched = completed;
        mismatched.edition_digest = [0x76; 32];
        write_terminal_finalization_intent(journal.path(), &mismatched, false)
            .expect("publish mismatched stale intent");
        let error = journal
            .replay()
            .expect_err("mismatched intent and completion must fail closed");
        assert!(error.to_string().contains("differs"), "{error}");
        assert!(terminal_finalization_intent_path(journal.path()).exists());
    }

    #[test]
    fn structured_completion_rejects_partial_progress_and_replica_c_mismatch() {
        let tape_uuid = [0x76; 16];
        let prior = record(tape_uuid);
        let mut terminal =
            structured_terminal_record(&prior, completed_finalization_intent(tape_uuid));
        terminal
            .terminal_finalization
            .as_mut()
            .expect("completion")
            .progress = TerminalFinalizationProgress::AfterReplicaB;
        let partial = validate_next_record(Some(&prior), &terminal)
            .expect_err("partial terminal progress must not become completion");
        assert!(partial.to_string().contains("AfterReplicaC"), "{partial}");

        terminal = structured_terminal_record(&prior, completed_finalization_intent(tape_uuid));
        terminal
            .barrier_bundle
            .as_mut()
            .expect("final component")
            .entries[0]
            .canonical_metadata_hash = Some([0x77; 32]);
        let mismatch = validate_next_record(Some(&prior), &terminal)
            .expect_err("replica C digest mismatch must fail closed");
        assert!(mismatch.to_string().contains("replica C"), "{mismatch}");
    }

    #[test]
    fn legacy_checkpoint_json_without_completion_field_remains_readable() {
        let tape_uuid = [0x77; 16];
        let mut value = serde_json::to_value(record(tape_uuid)).expect("encode checkpoint JSON");
        value
            .as_object_mut()
            .expect("checkpoint JSON object")
            .remove("terminal_finalization");
        let decoded: CheckpointJournalRecord =
            serde_json::from_value(value).expect("decode legacy checkpoint JSON");
        assert_eq!(decoded.terminal_finalization, None);
        validate_next_record(None, &decoded).expect("legacy ordinary checkpoint remains valid");
    }

    #[test]
    fn structured_finalization_rejects_pool_guard_and_corrupt_frame_before_motion() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x72; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        journal
            .append(&record(tape_uuid))
            .expect("append ordinary checkpoint prefix");
        let mut mismatch = finalization_intent(tape_uuid);
        mismatch
            .manual
            .as_mut()
            .expect("manual identity")
            .expected_pool_id = None;
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        let error = lease
            .begin_terminal_finalization(&mismatch)
            .expect_err("pooled omission must fail before publication");
        assert!(error.to_string().contains("pool guard"), "{error}");
        assert!(!terminal_finalization_intent_path(journal.path()).exists());

        let intent = finalization_intent(tape_uuid);
        lease
            .begin_terminal_finalization(&intent)
            .expect("publish valid intent");
        drop(lease);
        let intent_path = terminal_finalization_intent_path(journal.path());
        let mut bytes = fs::read(&intent_path).expect("read finalization frame");
        bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
        fs::write(&intent_path, &bytes).expect("write legacy finalization frame version");
        let error = journal
            .terminal_finalization_intent()
            .expect_err("legacy intent version must fail closed");
        assert!(
            error.to_string().contains("unsupported version 1"),
            "{error}"
        );
        bytes[8..10].copy_from_slice(&CHECKPOINT_FINALIZATION_INTENT_VERSION.to_le_bytes());
        bytes[14] ^= 1;
        fs::write(&intent_path, bytes).expect("corrupt finalization frame");
        let error = journal
            .terminal_finalization_intent()
            .expect_err("corrupt finalization intent must fail closed");
        assert!(error.to_string().contains("CRC"), "{error}");
    }

    #[test]
    fn no_parity_terminal_source_streams_dense_map_and_exact_object_bijection() {
        let tape_uuid = [0x73; 16];
        let records = vec![record(tape_uuid)];
        let mut source = CheckpointTerminalIndexRecordSource::new(&records, None)
            .expect("validate no-parity terminal source");
        assert_eq!(
            source.summary(),
            TerminalIndexAuthoritySummary {
                scope: remanence_parity::TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 2,
                    total_data_ordinals: 3,
                    highest_protected_ordinal: 0,
                },
                counts: remanence_parity::TapeIndexReplicaCounts {
                    structural_entry_count: 2,
                    object_row_count: 1,
                },
            }
        );
        let mut map = Vec::new();
        remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
            &mut source,
            &mut |entry| {
                map.push((entry.tape_file_number, entry.kind, entry.block_count));
                Ok(())
            },
        )
        .expect("stream structural map");
        assert_eq!(
            map,
            vec![
                (0, remanence_parity::TapeIndexReplicaFileKind::Bootstrap, 1,),
                (1, remanence_parity::TapeIndexReplicaFileKind::Object, 3,),
            ]
        );
        let mut object_rows = Vec::new();
        remanence_parity::TapeIndexReplicaRecordSource::visit_object_rows(
            &mut source,
            &mut |row| {
                object_rows.push((row.tape_file_number, row.stored_block_count));
                Ok(())
            },
        )
        .expect("stream Object rows");
        assert_eq!(object_rows, vec![(1, 3)]);

        let mut mismatched = records;
        mismatched[0].objects[0]
            .object_recovery_row
            .tape_file_number = 2;
        let error = CheckpointTerminalIndexRecordSource::new(&mismatched, None)
            .expect_err("map-to-row mismatch must fail before motion");
        assert!(error.to_string().contains("row/map mismatch"), "{error}");
    }

    #[test]
    fn replay_backed_no_parity_terminal_source_replays_each_replica_pass() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x74; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        lease
            .append(&record(tape_uuid))
            .expect("append first checkpoint");
        lease
            .append(&second_record(tape_uuid))
            .expect("append second checkpoint");

        let mut source = CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(&lease)
            .expect("validate bounded checkpoint replay");
        assert_eq!(
            source.summary(),
            TerminalIndexAuthoritySummary {
                scope: remanence_parity::TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 3,
                    total_data_ordinals: 6,
                    highest_protected_ordinal: 0,
                },
                counts: remanence_parity::TapeIndexReplicaCounts {
                    structural_entry_count: 3,
                    object_row_count: 2,
                },
            }
        );
        for _replica in 0..3 {
            let mut map_files = Vec::new();
            remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
                &mut source,
                &mut |entry| {
                    map_files.push(entry.tape_file_number);
                    Ok(())
                },
            )
            .expect("replay one structural pass");
            assert_eq!(map_files, vec![0, 1, 2]);

            let mut object_files = Vec::new();
            remanence_parity::TapeIndexReplicaRecordSource::visit_object_rows(
                &mut source,
                &mut |row| {
                    object_files.push(row.tape_file_number);
                    Ok(())
                },
            )
            .expect("replay one Object-row pass");
            assert_eq!(object_files, vec![1, 2]);
        }
    }

    #[test]
    fn owned_no_parity_snapshot_allows_checkpoint_append_and_keeps_frozen_rows() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x7D; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        lease
            .append(&record(tape_uuid))
            .expect("append first checkpoint");
        let mut source = CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(&lease)
            .expect("freeze bounded checkpoint source");

        lease
            .append(&second_record(tape_uuid))
            .expect("owned source must not borrow the mutable checkpoint lease");
        assert_eq!(source.summary().counts.structural_entry_count, 2);
        let mut files = Vec::new();
        remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
            &mut source,
            &mut |entry| {
                files.push(entry.tape_file_number);
                Ok(())
            },
        )
        .expect("replay frozen prefix after later append");
        assert_eq!(files, vec![0, 1]);
    }

    #[test]
    fn planned_prefix_snapshot_survives_durable_parity_transition_without_duplication() {
        let dir = tempfile::tempdir().expect("temporary authority directory");
        let tape_uuid = [0x7E; 16];
        let record = partial_epoch_parity_record(tape_uuid);
        let scheme = record.scheme.clone().expect("parity scheme");
        let checkpoint_journal =
            FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open checkpoint journal");
        let mut checkpoint_lease = checkpoint_journal
            .acquire_exclusive()
            .expect("acquire checkpoint lease");
        checkpoint_lease
            .append(&record)
            .expect("append checkpoint authority");

        let parity_path = dir.path().join("owned-planned-prefix.remjournal");
        let mut parity = remanence_parity::FileTapeFileJournal::open(
            &parity_path,
            tape_uuid,
            record.block_size,
            scheme,
        )
        .expect("open trusted parity journal");
        let bot = remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::BotBootstrap,
            entries: vec![parity_entry(0, remanence_parity::TapeFileKind::Bootstrap)],
            highest_protected_ordinal: 0,
            total_committed_ordinals: 0,
        };
        remanence_parity::TapeFileJournal::commit_bundle(&mut parity, &bot)
            .expect("commit BOT Bootstrap");
        for bundle in &record.object_tape_file_bundles {
            remanence_parity::TapeFileJournal::commit_bundle(&mut parity, bundle)
                .expect("commit Object bundle");
        }
        let barrier = record.barrier_bundle.as_ref().expect("checkpoint barrier");
        remanence_parity::TapeFileJournal::commit_bundle(&mut parity, barrier)
            .expect("commit checkpoint sidecar bundle");
        let checkpoint = remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::CheckpointedThrough,
            entries: Vec::new(),
            highest_protected_ordinal: barrier.highest_protected_ordinal,
            total_committed_ordinals: barrier.total_committed_ordinals,
        };
        remanence_parity::TapeFileJournal::commit_bundle(&mut parity, &checkpoint)
            .expect("commit checkpoint watermark");

        let prefix = terminal_prefix_plan_for_parity_record(&record);
        let mut planned =
            CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
                &checkpoint_lease,
                &parity,
                &prefix,
            )
            .expect("freeze base plus virtual TerminalPrefix");
        assert_eq!(planned.summary().counts.structural_entry_count, 4);
        assert_eq!(
            planned
                .replay_metrics()
                .expect("owned snapshot metrics")
                .parity
                .expect("parity metrics")
                .validation_passes,
            1
        );

        remanence_parity::TapeFileJournal::commit_bundle(&mut parity, &prefix.committed_bundle)
            .expect("simulate crash after the planned TerminalPrefix journal append");
        let mut orphan_planned =
            CheckpointTerminalIndexRecordSource::new_replay_backed_with_planned_terminal_prefix(
                &checkpoint_lease,
                &parity,
                &prefix,
            )
            .expect("exact planned TerminalPrefix orphan retains the bounded base authority");
        let mut orphan_planned_files = Vec::new();
        remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
            &mut orphan_planned,
            &mut |entry| {
                orphan_planned_files.push(entry.tape_file_number);
                Ok(())
            },
        )
        .expect("replay base plus orphaned planned prefix exactly once");
        assert_eq!(orphan_planned_files, vec![0, 1, 2, 3]);

        remanence_parity::TapeFileJournal::commit_terminal_prefix_transition(
            &mut parity,
            &prefix.committed_bundle,
            &remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::CheckpointedThrough,
                entries: Vec::new(),
                highest_protected_ordinal: prefix.committed_bundle.highest_protected_ordinal,
                total_committed_ordinals: prefix.committed_bundle.total_committed_ordinals,
            },
        )
        .expect("owned planned source must not borrow mutable parity journal");

        let mut planned_files = Vec::new();
        remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
            &mut planned,
            &mut |entry| {
                planned_files.push(entry.tape_file_number);
                Ok(())
            },
        )
        .expect("replay frozen base plus one virtual prefix");
        assert_eq!(planned_files, vec![0, 1, 2, 3]);

        let replica_a = remanence_parity::CommittedBundle {
            kind: remanence_parity::CommittedBundleKind::TerminalComponent,
            entries: vec![remanence_parity::TapeFileEntry {
                tape_file_number: prefix.tail_start_tape_file_number,
                kind: remanence_parity::TapeFileKind::TapeIndexReplica,
                block_count: 2,
                physical_start_hint: Some(prefix.tail_start_lba),
                object_id: None,
                first_parity_data_ordinal: None,
                epoch_id: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                canonical_metadata_hash: Some([0x71; 32]),
                object_recovery_row: None,
            }],
            highest_protected_ordinal: prefix.committed_bundle.highest_protected_ordinal,
            total_committed_ordinals: prefix.committed_bundle.total_committed_ordinals,
        };
        remanence_parity::TapeFileJournal::commit_terminal_component_transition(
            &mut parity,
            &replica_a,
            &remanence_parity::CommittedBundle {
                kind: remanence_parity::CommittedBundleKind::CheckpointedThrough,
                entries: Vec::new(),
                highest_protected_ordinal: replica_a.highest_protected_ordinal,
                total_committed_ordinals: replica_a.total_committed_ordinals,
            },
        )
        .expect("append later replica-A progress");

        let mut durable =
            CheckpointTerminalIndexRecordSource::new_replay_backed_after_terminal_prefix(
                &checkpoint_lease,
                &parity,
                &prefix,
            )
            .expect("freeze durable prefix while excluding later component progress");
        let mut durable_files = Vec::new();
        remanence_parity::TapeIndexReplicaRecordSource::visit_structural_entries(
            &mut durable,
            &mut |entry| {
                durable_files.push(entry.tape_file_number);
                Ok(())
            },
        )
        .expect("replay durable prefix exactly once");
        assert_eq!(durable_files, planned_files);
    }

    #[test]
    fn high_count_checkpoint_replay_reports_one_pass_and_bounded_live_rows() {
        const CHECKPOINT_COUNT: u64 = 128;
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x75; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        let mut prior = None;
        for index in 0..CHECKPOINT_COUNT {
            let mut next = record(tape_uuid);
            let ordinal = index + 1;
            let object_uuid = uuid::Uuid::from_u128(u128::from(ordinal));
            next.ordinal = ordinal;
            next.committed_object_count = ordinal;
            next.eod_lba = ordinal * 4 + 2;
            next.batch_id = object_uuid.into_bytes();
            next.next_tape_file_number = ordinal + 1;
            next.objects[0].object.object_id = object_uuid.to_string();
            next.objects[0].copy.object_id = object_uuid.to_string();
            next.objects[0].copy.tape_file_number = ordinal;
            next.objects[0].fresh_tape = ordinal == 1;
            next.objects[0].total_committed_ordinals = ordinal * 3;
            next.objects[0].object_recovery_row.tape_file_number = ordinal;
            next.objects[0].object_recovery_row.object_id = object_uuid.to_string().into_bytes();
            lease.append(&next).expect("append high-count checkpoint");
            prior = Some(next);
        }
        assert_eq!(
            prior
                .expect("high-count fixture has a final record")
                .ordinal,
            CHECKPOINT_COUNT
        );

        let metrics = lease
            .bounded_replay_metrics()
            .expect("measure bounded checkpoint replay");
        assert_eq!(metrics.replay_passes, 1);
        assert_eq!(metrics.frame_count, CHECKPOINT_COUNT);
        assert!(metrics.peak_frame_payload_bytes <= MAX_CHECKPOINT_RECORD_LEN);
        assert_eq!(metrics.peak_live_record_count, 2);
        assert_eq!(metrics.peak_live_object_rows, 2);
        assert!(metrics.peak_live_object_rows < CHECKPOINT_COUNT);

        let mut source = CheckpointTerminalIndexRecordSource::new_replay_backed_no_parity(&lease)
            .expect("validate high-count replay-backed source");
        assert_eq!(
            source.summary().counts,
            remanence_parity::TapeIndexReplicaCounts {
                structural_entry_count: CHECKPOINT_COUNT + 1,
                object_row_count: CHECKPOINT_COUNT,
            }
        );
        let mut emitted = 0u64;
        remanence_parity::TapeIndexReplicaRecordSource::visit_object_rows(&mut source, &mut |_| {
            emitted += 1;
            Ok(())
        })
        .expect("stream high-count Object rows");
        assert_eq!(emitted, CHECKPOINT_COUNT);
    }

    #[test]
    fn final_edition_reconstruction_binds_scope_counts_and_diagnostics() {
        let tape_uuid = [0x7C; 16];
        let records = vec![record(tape_uuid)];
        let mut source = CheckpointTerminalIndexRecordSource::new(&records, None)
            .expect("validate terminal source");
        let summary = source.summary();
        let replica_records = remanence_parity::checked_tape_index_replica_layout(
            records[0].block_size,
            summary.counts,
        )
        .expect("replica layout")
        .replica_record_count;
        let layout = remanence_parity::TerminalTailLayout::new(
            0,
            records[0].block_size,
            records[0].next_tape_file_number,
            records[0].eod_lba,
            replica_records,
            4_096,
        )
        .expect("terminal layout");
        let descriptor = remanence_parity::TapeIndexEditionDescriptor {
            tape_uuid,
            edition_id: [0x64; 16],
            edition_sequence: 1,
            scope: summary.scope,
            counts: summary.counts,
            block_size: records[0].block_size,
            compression_enabled: false,
            writer_version: "remanence-test".to_string(),
            write_timestamp: "2026-08-09T00:00:00Z".to_string(),
            terminal_layout: layout,
        };
        let planned = remanence_parity::plan_tape_index_edition(descriptor, &mut source)
            .expect("plan canonical edition");
        let mut intent = finalization_intent(tape_uuid);
        intent.layout = TerminalFinalizationLayout::try_from(layout).expect("persist layout");
        intent.edition_digest = planned.edition_digest;

        let reconstructed = source
            .reconstruct_final_edition(&intent)
            .expect("reconstruct exact persisted edition");
        assert_eq!(reconstructed, planned);

        intent.writer_version = "remanence-upgraded".to_string();
        let mismatch = source
            .reconstruct_final_edition(&intent)
            .expect_err("changed writer diagnostics must change the edition digest");
        assert!(mismatch.to_string().contains("digest"), "{mismatch}");
    }

    #[test]
    fn terminal_checkpoint_names_first_free_file_after_replica_c() {
        let tape_uuid = [0x26; 16];
        let prior = record(tape_uuid);
        let mut terminal =
            structured_terminal_record(&prior, completed_finalization_intent(tape_uuid));
        validate_next_record(Some(&prior), &terminal)
            .expect("replica C completion names the first free tape file");

        let replica_c = terminal
            .terminal_finalization
            .as_ref()
            .expect("structured completion")
            .layout
            .components[4];
        terminal.next_tape_file_number = replica_c.tape_file_number;
        let mismatch = validate_next_record(Some(&prior), &terminal)
            .expect_err("replica C itself is not the next free tape file");
        assert!(mismatch.to_string().contains("replica C"), "{mismatch}");
    }

    #[test]
    fn checkpoint_batch_is_one_integrity_frame_and_torn_batch_fails_closed() {
        let dir = tempfile::tempdir().expect("temporary checkpoint directory");
        let tape_uuid = [0x27; 16];
        let journal = FileCheckpointJournal::open(dir.path(), tape_uuid).expect("open journal");
        let first = record(tape_uuid);
        let second = second_record(tape_uuid);
        let mut lease = journal.acquire_exclusive().expect("acquire journal lease");
        lease
            .append_batch(&[first.clone(), second.clone()])
            .expect("append checkpoint transition");
        drop(lease);
        assert_eq!(
            journal.replay().expect("replay checkpoint transition"),
            vec![first, second]
        );

        let file = OpenOptions::new()
            .write(true)
            .open(journal.path())
            .expect("open journal for crash cut");
        let len = file.metadata().expect("stat journal").len();
        file.set_len(len - 1).expect("tear transition checksum");
        file.sync_all().expect("sync torn transition");
        let err = journal
            .replay()
            .expect_err("a torn multi-record transition must fail closed");
        assert!(err.to_string().contains("torn trailing frame"), "{err}");
    }

    #[test]
    fn validation_rejects_parity_scheme_change_between_checkpoints() {
        let tape_uuid = [0x23; 16];
        let mut previous = record(tape_uuid);
        previous.scheme = Some(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("checkpoint-scheme-a"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 3,
        });
        let mut next = second_record(tape_uuid);
        next.scheme = Some(remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_static("checkpoint-scheme-b"),
            data_blocks_per_stripe: 4,
            parity_blocks_per_stripe: 2,
            stripes_per_neighborhood: 3,
        });

        let err = validate_next_record(Some(&previous), &next)
            .expect_err("one tape checkpoint journal cannot change parity schemes");
        assert!(err.to_string().contains("scheme changed"), "{err}");
    }

    #[test]
    fn validation_accepts_non_uuid_object_id() {
        // object_id is opaque UTF-8, 1-64 bytes (REM-OBJECT 4.5.1); the state layer must
        // not require it to parse as a UUID (task #28 — vestigial UUID guards removed).
        let tape_uuid = [0x24; 16];
        let mut rec = record(tape_uuid);
        let opaque = "accession-2026-0007";
        rec.objects[0].object.object_id = opaque.to_string();
        rec.objects[0].copy.object_id = opaque.to_string();
        rec.objects[0].object_recovery_row.object_id = opaque.as_bytes().to_vec();

        validate_next_record(None, &rec).expect("a non-UUID opaque object_id must validate");
    }
}
