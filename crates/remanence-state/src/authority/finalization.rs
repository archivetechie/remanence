//! Format-neutral finalization plans and recovery projections shared by authority backends.
use crate::StateError;
use remanence_parity::authority::{
    ManualTerminalFinalizationIdentity, TerminalFinalizationTrigger,
};
use serde::{Deserialize, Serialize};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Immutable planned media prefix; no host-log discriminator is exposed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityPrefixPlan {
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
    pub sidecar_directory_entries: Vec<remanence_parity::SidecarEpochDirectoryEntry>,
    /// Exact TerminalPrefix rows and post-prefix W/T authority.
    pub publication: remanence_parity::authority::FilePublication,
}

/// Durable plan and current component progress, independent of storage framing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorityFinalization {
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
    pub terminal_prefix: Option<AuthorityPrefixPlan>,
    /// Immutable five-component physical plan.
    pub layout: TerminalFinalizationLayout,
}

/// Validated recovery projection returned by the one retained writer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizationRecovery {
    /// Durable semantic commits, including sealed completion if present.
    pub commits: Vec<super::AuthorityCommit>,
    /// Pending plan after reconciling any already sealed completion.
    pub finalization: Option<AuthorityFinalization>,
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
    pub(crate) fn to_parity_layout(&self) -> remanence_parity::TerminalTailLayout {
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

    pub(crate) fn validate(&self) -> Result<(), StateError> {
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

impl AuthorityPrefixPlan {
    pub(crate) fn validate(&self) -> Result<(), StateError> {
        self.diagnostics.validate().map_err(|error| {
            StateError::JournalReplayFailed(format!("invalid terminal prefix diagnostics: {error}"))
        })?;

        self.publication.validate().map_err(|error| {
            StateError::JournalReplayFailed(format!(
                "terminal finalization prefix bundle is invalid: {error}"
            ))
        })?;
        if self.publication.purpose != remanence_parity::authority::FilePurpose::TerminalPrefix {
            return Err(StateError::JournalReplayFailed(format!(
                "terminal finalization prefix uses {:?} bundle kind instead of TerminalPrefix",
                self.publication.purpose
            )));
        }
        if self.publication.protected > self.publication.total {
            return Err(StateError::JournalReplayFailed(
                "terminal finalization prefix W exceeds T".to_string(),
            ));
        }

        let mut expected_file = self.start_tape_file_number;
        let mut expected_lba = self.start_lba;
        let mut observed_parity_map = None;
        for entry in &self.publication.files {
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
                    if start >= end || end > self.publication.protected {
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
                directory_scope_total_data_ordinals: self.publication.total,
                directory_scope_highest_protected_ordinal: self.publication.protected,
                is_final_directory: true,
                entries: self.sidecar_directory_entries.clone(),
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
impl AuthorityFinalization {
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
            if prefix.diagnostics.writer_version != self.writer_version
                || prefix.diagnostics.write_timestamp != self.write_timestamp
            {
                return Err(StateError::JournalReplayFailed(
                    "terminal edition diagnostics differ from prefix plan".into(),
                ));
            }
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
}
