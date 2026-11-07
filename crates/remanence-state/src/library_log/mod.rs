//! Library authority v1: typed records, deterministic CBOR, causal fold and segmented storage.
//! This module has no production caller until the B4b authority cutover.
//!
//! Fold retention is proportional to resources and open operations. Terminal
//! operation chains and load-attempt indexes are removed. Lifetime idempotency
//! retains SHA-256(length-prefixed actor UTF-8, little-endian u16 kind, key) ->
//! (operation UUID, request fingerprint): 80 bytes of key/value data per keyed
//! operation, plus measured hash-table overhead. Rare crash supersedes and
//! unprotected acceptances retain one lifetime entry each. Pending is independently capped.
//! Wrap-map descriptors remain in source records for the projector; the fold keeps
//! only the digest and calibration generation per tape.
//!
//! Tag 20 (`DriveHealthObserved`) compares a non-null `(session_id, trigger)` only
//! with the drive's latest retained snapshot: an identical body is a no-op and a
//! different body is `ConflictingDuplicate`. The fold does not enforce lifetime
//! uniqueness. B4's health projector must keep the `(session_id, trigger)` unique
//! key with first-wins: an identical later body is ignored; a different later body
//! is reported as a projection conflict and never applied. Unkeyed (null-session)
//! observations remain independent snapshots. Counters are cumulative per mount.
pub mod authority;
pub use authority::{
    AppliedLibraryRecord, AuthorityError, AuthorityReceipt, FinalizationView, LibraryAuthority,
    LibrarySnapshot,
};
pub mod codec;
#[cfg(test)]
mod differential_tests;
pub mod fold;
mod index;
pub mod native_view;
#[cfg(test)]
mod reference_fixtures;
pub mod store;
#[cfg(test)]
mod tests;
mod wire;
pub use crate::audit::{AuditActor, SourceLayer};
pub use crate::log_frame::cbor::Bounded;
pub use crate::{GenerationSource, LtoGen};
pub use fold::{
    FoldError, LibraryFold, LibraryHistory, TapeLookup, TapeView, MAX_PENDING_BYTES,
    MAX_PENDING_RECORDS,
};
pub use store::{LibraryLog, Receipt};
pub use wire::{Blob, Negative, TextValue, Unit};
/// Binary UUID.
pub type Id = [u8; 16];
/// Stable inventory identity used by cleaning registration; no BOT is required.
pub fn cleaning_cartridge_ref(barcode: &LabelText) -> TapeRef {
    let name = format!("remanence:cleaning-cartridge:{}", barcode.as_str());
    TapeRef {
        uuid: *uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, name.as_bytes()).as_bytes(),
        incarnation: 1,
    }
}
/// SHA-256 digest.
pub type Hash = [u8; 32];
/// Bounded UTF-8 presentation or identifier text.
pub type Text = TextValue<4096>;
/// SCSI volume-tag identifier.
pub type LabelText = TextValue<32>;
/// Envelope fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    /// CBOR key 0: `Id`.
    pub record_uuid: Id,
    /// CBOR key 1: `u64`.
    pub timestamp_unix_ns: u64,
    /// CBOR key 2: `Text`.
    pub host_id: Text,
    /// CBOR key 3: `u32`.
    pub process_id: u32,
    /// CBOR key 4: `Text`.
    pub software_build: Text,
    /// CBOR key 5: `AuditActor`.
    pub actor: AuditActor,
    /// CBOR key 6: `SourceLayer`.
    pub source_layer: SourceLayer,
    /// CBOR key 7: `Option<Id>`.
    pub operation_id: Option<Id>,
    /// CBOR key 8: `Option<Id>`.
    pub session_id: Option<Id>,
    /// CBOR key 9: `Option<Id>`.
    pub idempotency_key: Option<Id>,
}
/// TapeRef fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq, Copy, PartialOrd, Ord, Hash)]
pub struct TapeRef {
    /// CBOR key 0: `Id`.
    pub uuid: Id,
    /// CBOR key 1: `u64`.
    pub incarnation: u64,
}
/// TapeAt fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeAt {
    /// CBOR key 0: `TapeRef`.
    pub tape: TapeRef,
    /// CBOR key 1: `u64`.
    pub sequence: u64,
    /// CBOR key 2: `Hash`.
    pub record_digest: Hash,
}
/// DecisionRef fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq, Copy, PartialOrd, Ord, Hash)]
pub struct DecisionRef {
    /// CBOR key 0: `u64`.
    pub sequence: u64,
    /// CBOR key 1: `Id`.
    pub record_uuid: Id,
}
/// GenerationChange fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq, Copy, PartialOrd, Ord, Hash)]
pub struct GenerationChange {
    /// CBOR key 0: `u64`.
    pub expected: u64,
    /// CBOR key 1: `u64`.
    pub next: u64,
}
/// CompletionEvidence fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionEvidence {
    /// CBOR key 0: `Dispatch`.
    pub dispatch: Dispatch,
    /// CBOR key 1: `Completion`.
    pub completion: Completion,
    /// CBOR key 2: `Option<u8>`.
    pub scsi_status: Option<u8>,
    /// CBOR key 3: `Option<u16>`.
    pub host_status: Option<u16>,
    /// CBOR key 4: `Option<u16>`.
    pub driver_status: Option<u16>,
    /// CBOR key 5: `Option<Blob<252>>`.
    pub sense: Option<Blob<252>>,
    /// CBOR key 6: `Option<u64>`.
    pub residual: Option<u64>,
}
/// Bay fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bay {
    /// CBOR key 0: `Text`.
    pub library_serial: Text,
    /// CBOR key 1: `u16`.
    pub bay: u16,
}
/// Standalone fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Standalone {
    /// CBOR key 0: `Text`.
    pub drive_serial: Text,
}
/// PriorHolder fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PriorHolder {
    /// CBOR key 0: `TapeRef`.
    pub tape: TapeRef,
    /// CBOR key 1: `GenerationChange`.
    pub change: GenerationChange,
}
/// Medium identity; generation is meaningful only for LTO.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediumFamily {
    /// Wire code 0, with an LTO generation payload.
    Lto(LtoGen),
    /// Wire code 1, with a null payload.
    Disk,
}
/// Medium spec recorded at provisioning and revised under media_revision.
/// Geometry and capabilities are fixed per incarnation. Capacity and source are revisable;
/// an LTO generation correction additionally requires that no committed spec in this
/// incarnation has had measured provenance.
/// Rerecord implies regenerative use; role is never stored separately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediumSpec {
    /// CBOR key 0: medium identity.
    pub family: MediumFamily,
    /// CBOR key 1: usable capacity in logical bytes.
    pub usable_logical_bytes: u64,
    /// CBOR key 2: fixed block size, the sole geometry field.
    pub record_size: u32,
    /// CBOR key 3: identical-content re-record capability.
    pub rerecord: bool,
    /// CBOR key 4: lifecycle operations capability (never inferred from family).
    pub lifecycle_ops: bool,
    /// CBOR key 5: provenance of this spec; Measured attests a fresh measurement.
    pub source: GenerationSource,
}
/// Adoption fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adoption {
    /// CBOR key 0: `Hash`.
    pub request_fingerprint: Hash,
    /// CBOR key 1: `Id`.
    pub operation: Id,
    /// CBOR key 2: `Id`.
    pub idempotency_key: Id,
    /// CBOR key 3: `PhysicalTail`.
    pub physical_tail: PhysicalTail,
    /// CBOR key 4: `AdoptedState`.
    pub state: AdoptedState,
    /// CBOR key 5: `Option<Text>`.
    pub library_revision: Option<Text>,
    /// CBOR key 6: `Option<u16>`.
    pub home_slot: Option<u16>,
    /// CBOR key 7: `Text`.
    pub drive_serial: Text,
    /// CBOR key 8: `bool`.
    pub identity_only: bool,
    /// CBOR key 9: `bool`.
    pub bootstrap_drive_compression: bool,
    /// CBOR key 10: `bool`.
    pub configured_drive_compression: bool,
}
/// Load fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Load {
    /// CBOR key 0: `DriveKey`.
    pub endpoint: DriveKey,
    /// CBOR key 1: `u32`.
    pub attempt: u32,
}
/// Unload fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unload {
    /// CBOR key 0: `DriveKey`.
    pub endpoint: DriveKey,
}
/// Move fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Move {
    /// CBOR key 0: `Text`.
    pub library_serial: Text,
    /// CBOR key 1: `u16`.
    pub source: u16,
    /// CBOR key 2: `u16`.
    pub destination: u16,
}
/// Element fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    /// CBOR key 0: `u16`.
    pub address: u16,
    /// CBOR key 1: `ElementKind`.
    pub kind: ElementKind,
    /// CBOR key 2: `Option<LabelText>`.
    pub barcode: Option<LabelText>,
    /// CBOR key 3: `bool`.
    pub full: bool,
}
/// VerificationCursor fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq, Copy, PartialOrd, Ord, Hash)]
pub struct VerificationCursor {
    /// CBOR key 0: `u16`.
    pub partition: u16,
    /// CBOR key 1: `u64`.
    pub file: u64,
    /// CBOR key 2: `u64`.
    pub lba: u64,
    /// CBOR key 3: `u64`.
    pub ordinal: u64,
    /// CBOR key 4: `u32`.
    pub part: u32,
}
/// DamagedRange fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DamagedRange {
    /// CBOR key 0: `u16`.
    pub partition: u16,
    /// CBOR key 1: `u64`.
    pub start_lba: u64,
    /// CBOR key 2: `u64`.
    pub end_lba: u64,
}
/// Unprotected fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unprotected {
    /// CBOR key 0: `u64`.
    pub w: u64,
    /// CBOR key 1: `u64`.
    pub t: u64,
}
/// PoolDefinition fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolDefinition {
    /// CBOR key 0: `Text`.
    pub id: Text,
    /// CBOR key 1: `Option<Text>`.
    pub display_name: Option<Text>,
    /// CBOR key 2: `Option<Text>`.
    pub copy_class: Option<Text>,
    /// CBOR key 3: `Option<Text>`.
    pub content_class: Option<Text>,
    /// CBOR key 4: `Option<Text>`.
    pub scheme: Option<Text>,
    /// CBOR key 5: `SelectionPolicy`.
    pub selection_policy: SelectionPolicy,
    /// CBOR key 6: `u64`.
    pub watermark_low_bits: u64,
    /// CBOR key 7: `u64`.
    pub watermark_high_bits: u64,
    /// CBOR key 8: `Option<u64>`.
    pub capacity_cap_bytes: Option<u64>,
    /// CBOR key 9: `u64`.
    pub block_size_bytes: u64,
    /// CBOR key 10: `u64`.
    pub min_object_size_bytes: u64,
}
/// PoolRule fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolRule {
    /// CBOR key 0: `Text`.
    pub prefix: Text,
    /// CBOR key 1: `Text`.
    pub pool_id: Text,
}
/// Definitions fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Definitions {
    /// CBOR key 0: `Bounded<PoolDefinition, 4096>`.
    pub pools: Bounded<PoolDefinition, 4096>,
    /// CBOR key 1: `Bounded<PoolRule, 4096>`.
    pub rules: Bounded<PoolRule, 4096>,
    /// CBOR key 2: `Bounded<Text, 256>`.
    pub cleaning_prefixes: Bounded<Text, 256>,
}
/// Reason fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reason {
    /// CBOR key 0: `Text`.
    pub reason: Text,
}
/// Annotation fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
    /// CBOR key 0: `Option<Text>`.
    pub purchase_date: Option<Text>,
    /// CBOR key 1: `Option<Text>`.
    pub warranty_until: Option<Text>,
    /// CBOR key 2: `Option<Text>`.
    pub cost: Option<Text>,
    /// CBOR key 3: `Option<Text>`.
    pub notes: Option<Text>,
}
/// Counters fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counters {
    /// CBOR key 0: `Option<u64>`.
    pub write_corrected: Option<u64>,
    /// CBOR key 1: `Option<u64>`.
    pub write_uncorrected: Option<u64>,
    /// CBOR key 2: `Option<u64>`.
    pub read_corrected: Option<u64>,
    /// CBOR key 3: `Option<u64>`.
    pub read_uncorrected: Option<u64>,
}
/// Register fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Register {
    /// CBOR key 0: `LabelText`.
    pub barcode: LabelText,
    /// CBOR key 1: `u16`.
    pub home_slot: u16,
}
/// Begin fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Begin {
    /// CBOR key 0: `CleanTrigger`.
    pub trigger: CleanTrigger,
}
/// Advance fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advance {
    /// CBOR key 0: `CleanPhase`.
    pub phase: CleanPhase,
    /// CBOR key 1: selected source slot, present exactly for MovingIn.
    pub home_slot: Option<u16>,
}
/// Finish fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finish {
    /// CBOR key 0: `CleanResult`.
    pub result: CleanResult,
    /// CBOR key 1: `bool`.
    pub verified_credit: bool,
}
/// AlarmRaise fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlarmRaise {
    /// CBOR key 0: `AlarmKind`.
    pub kind: AlarmKind,
    /// CBOR key 1: `Severity`.
    pub severity: Severity,
    /// CBOR key 2: `Text`.
    pub reason: Text,
}
/// Accepted fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    /// CBOR key 0: `Text`.
    pub actor_fingerprint: Text,
    /// CBOR key 1: `Option<Id>`.
    pub idempotency_key: Option<Id>,
    /// CBOR key 2: `Hash`.
    pub request_fingerprint: Hash,
    /// CBOR key 3: `Option<Text>`.
    pub subject: Option<Text>,
}
/// Response fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// CBOR key 0: `Option<Hash>`.
    pub response_fingerprint: Option<Hash>,
}
/// Failed fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failed {
    /// CBOR key 0: `Option<Retry>`.
    pub retry: Option<Retry>,
    /// CBOR key 1: `Option<u32>`.
    pub refusal_code: Option<u32>,
    /// CBOR key 2: `Text`.
    pub summary: Text,
}
/// Stable refusal codes shared by Failed and RefusalRecorded; zero is never emitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum RefusalCode {
    /// Refusal code 1.
    TapeLogBehindProjection = 1,
    /// Refusal code 2.
    ContradictedTapeAdmission = 2,
    /// Refusal code 3.
    ClosedOperationRetry = 3,
    /// Refusal code 4.
    RetainedIntentForeignOwner = 4,
    /// Refusal code 5.
    StandaloneFenceUnrecorded = 5,
    /// Refusal code 6.
    ReconcileTapeDeferred = 6,
    /// Refusal code 7.
    UnknownGenerationAdoption = 7,
    /// Refusal code 8.
    DriveIdentityConflictLatch = 8,
    /// Refusal code 9.
    ForeignDrivePollDisabled = 9,
    /// Refusal code 10.
    DriveStewardshipDeferred = 10,
    /// Refusal code 11.
    CheckpointBarrierAlarmUnrecorded = 11,
    /// Refusal code 12.
    DriveFenced = 12,
    /// Refusal code 13.
    TailRepairDeferred = 13,
    /// Refusal code 14.
    JournalImportDeferred = 14,
}
impl RefusalCode {
    /// Decode an allocated refusal code.
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::TapeLogBehindProjection),
            2 => Some(Self::ContradictedTapeAdmission),
            3 => Some(Self::ClosedOperationRetry),
            4 => Some(Self::RetainedIntentForeignOwner),
            5 => Some(Self::StandaloneFenceUnrecorded),
            6 => Some(Self::ReconcileTapeDeferred),
            7 => Some(Self::UnknownGenerationAdoption),
            8 => Some(Self::DriveIdentityConflictLatch),
            9 => Some(Self::ForeignDrivePollDisabled),
            10 => Some(Self::DriveStewardshipDeferred),
            11 => Some(Self::CheckpointBarrierAlarmUnrecorded),
            12 => Some(Self::DriveFenced),
            13 => Some(Self::TailRepairDeferred),
            14 => Some(Self::JournalImportDeferred),
            _ => None,
        }
    }
}
/// Cache fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cache {
    /// CBOR key 0: `Blob<4194304>`.
    pub descriptors: Blob<4194304>,
    /// CBOR key 1: `u64`.
    pub mapped_extent_lba: u64,
    /// CBOR key 2: `u64`.
    pub write_epoch: u64,
    /// CBOR key 3: `Hash`.
    pub digest: Hash,
}
/// RepairApplied fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairApplied {
    /// CBOR key 0: `Hash`.
    pub archived_prefix_digest: Hash,
    /// CBOR key 1: `u64`.
    pub retained_through: u64,
}
/// Recovered fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// CBOR key 0: `Hash`.
    pub source_digest: Hash,
}
/// AssignPool fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignPool {
    /// CBOR key 0: `TapeRef`.
    pub tape: TapeRef,
    /// CBOR key 1: `Option<Text>`.
    pub pool: Option<Text>,
    /// CBOR key 2: `GenerationChange`.
    pub change: GenerationChange,
    /// CBOR key 3: `AssignmentSource`.
    pub source: AssignmentSource,
    /// CBOR key 4: `Option<DecisionRef>`.
    pub configuration: Option<DecisionRef>,
}
/// Label fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Label {
    /// CBOR key 0: `TapeRef`.
    pub tape: TapeRef,
    /// CBOR key 1: `Option<LabelText>`.
    pub label: Option<LabelText>,
    /// CBOR key 2: `LabelProvenance`.
    pub provenance: LabelProvenance,
    /// CBOR key 3: `GenerationChange`.
    pub change: GenerationChange,
    /// CBOR key 4: `Option<PriorHolder>`.
    pub prior_holder: Option<PriorHolder>,
    /// CBOR key 5: `Option<Id>`.
    pub observed_bot_uuid: Option<Id>,
}
/// Retire fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Retire {
    /// CBOR key 0: `TapeRef`.
    pub tape: TapeRef,
    /// CBOR key 1: `RetireReason`.
    pub reason: RetireReason,
    /// CBOR key 2: `Option<u64>`.
    pub observed_tape_sequence: Option<u64>,
    /// CBOR key 3: `u64`.
    pub assignment_generation: u64,
    /// CBOR key 4: `Option<Text>`.
    pub detail: Option<Text>,
}
/// Reprovision fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reprovision {
    /// CBOR key 0: `TapeRef`.
    pub tape: TapeRef,
    /// CBOR key 1: `Option<TapeRef>`.
    pub predecessor: Option<TapeRef>,
    /// CBOR key 2: `ProvisionMode`.
    pub mode: ProvisionMode,
    /// CBOR key 3: `Option<LabelText>`.
    pub label: Option<LabelText>,
    /// CBOR key 4: `Option<LabelProvenance>`.
    pub label_provenance: Option<LabelProvenance>,
    /// CBOR key 5: `Option<ParityScheme>`.
    pub parity_scheme: Option<ParityScheme>,
    /// CBOR key 6: `MediumSpec`.
    pub medium: MediumSpec,
    /// CBOR key 7: `Option<Text>`.
    pub pool: Option<Text>,
    /// CBOR key 8: `Option<DriveKey>`.
    pub endpoint: Option<DriveKey>,
    /// CBOR key 9: `Option<Adoption>`.
    pub adoption: Option<Adoption>,
    /// CBOR key 10: optional source of the pool assignment.
    pub pool_source: Option<PoolSource>,
    /// CBOR key 11: optional initialization operation.
    pub operation: Option<Id>,
    /// CBOR key 12: overwritten BOT identity; evidence only.
    pub replaced_bot: Option<Id>,
}
/// MediumSpecSet fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediumSpecSet {
    /// CBOR key 0: `TapeRef`.
    pub tape: TapeRef,
    /// CBOR key 1: replacement medium spec.
    pub medium: MediumSpec,
    /// CBOR key 2: `GenerationChange` against media_revision.
    pub change: GenerationChange,
}
/// SupersedeTail fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupersedeTail {
    /// CBOR key 0: `TapeAt`.
    pub at: TapeAt,
    /// CBOR key 1: optional `Id`.
    pub operation: Option<Id>,
    /// CBOR key 2: optional `Id`.
    pub session_id: Option<Id>,
    /// CBOR key 3: optional `Hash`.
    pub preview_digest: Option<Hash>,
    /// CBOR key 4: `u64`.
    pub tail_file_count: u64,
}
/// AcceptUnprotectedFinalization fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptUnprotectedFinalization {
    /// CBOR key 0: `TapeAt`.
    pub(crate) at: TapeAt,
    /// CBOR key 1: `Id`.
    pub(crate) operation: Id,
    /// CBOR key 2: `u64`.
    pub(crate) w: u64,
    /// CBOR key 3: `u64`.
    pub(crate) t: u64,
    /// CBOR key 4: `Text`.
    pub(crate) reason: Text,
}
/// FenceRaised fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FenceRaised {
    /// CBOR key 0: `FenceKind`.
    pub kind: FenceKind,
    /// CBOR key 1: `FenceScope`.
    pub scope: FenceScope,
    /// CBOR key 2: `FenceCause`.
    pub cause: FenceCause,
    /// CBOR key 3: `Option<CompletionEvidence>`.
    pub evidence: Option<CompletionEvidence>,
    /// CBOR key 4: `Option<TapeAt>`.
    pub at: Option<TapeAt>,
    /// CBOR key 5: `Option<Id>`.
    pub operation: Option<Id>,
    /// CBOR key 6: presentation evidence in strictly ascending byte-key order.
    pub detail: Bounded<DiagnosticEntry, 256>,
}
impl From<&remanence_library::outcome::CompletionEvidence> for CompletionEvidence {
    fn from(value: &remanence_library::outcome::CompletionEvidence) -> Self {
        use remanence_library::outcome;
        Self {
            dispatch: match value.dispatch {
                outcome::Dispatch::Issued => Dispatch::Issued,
                outcome::Dispatch::NotIssued => Dispatch::NotIssued,
            },
            completion: match value.completion {
                outcome::Completion::Known => Completion::Known,
                outcome::Completion::Unknown => Completion::Unknown,
            },
            scsi_status: value.scsi_status,
            host_status: value.host_status,
            driver_status: value.driver_status,
            sense: value.sense.clone().map(Blob),
            residual: value.residual,
        }
    }
}
/// FenceReleased fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FenceReleased {
    /// CBOR key 0: FenceId wire sequence of the raise in this log.
    pub fence: u64,
    /// CBOR key 1: `Text`.
    pub evidence: Text,
    /// CBOR key 2: `bool`.
    pub forced: bool,
    /// CBOR key 3: presentation evidence in strictly ascending byte-key order.
    pub detail: Bounded<DiagnosticEntry, 256>,
}
/// MotionIntent fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MotionIntent {
    /// CBOR key 0: `Id`.
    pub command: Id,
    /// CBOR key 1: `Option<Id>`.
    pub parent_operation: Option<Id>,
    /// CBOR key 2: `Motion`.
    pub motion: Motion,
    /// CBOR key 3: `Option<TapeRef>`.
    pub tape: Option<TapeRef>,
    /// CBOR key 4: `Option<u64>`.
    pub inventory_generation: Option<u64>,
    /// CBOR key 5: `Option<u64>`.
    pub readiness_generation: Option<u64>,
}
/// MotionOutcome fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MotionOutcome {
    /// CBOR key 0: `DecisionRef`.
    pub intent: DecisionRef,
    /// CBOR key 1: `MotionResult`.
    pub result: MotionResult,
    /// CBOR key 2: `CompletionEvidence`.
    pub evidence: CompletionEvidence,
    /// CBOR key 3: `Option<Confirmation>`.
    pub confirmation: Option<Confirmation>,
}
/// InventoryReconciled fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryReconciled {
    /// CBOR key 0: library or standalone observation subject.
    pub subject: InventorySubject,
    /// CBOR key 1: `u64`.
    pub observation_generation: u64,
    /// CBOR key 2: `Bounded<DecisionRef, 1024>`.
    pub resolves: Bounded<DecisionRef, 1024>,
    /// CBOR key 3: `bool`.
    pub complete: bool,
    /// CBOR key 4: `Bounded<Element, 65535>`.
    pub elements: Bounded<Element, 65535>,
}
/// VerificationProgress fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationProgress {
    /// CBOR key 0: `Id`.
    pub(crate) operation: Id,
    /// CBOR key 1: `TapeAt`.
    pub(crate) at: TapeAt,
    /// CBOR key 2: `Hash`.
    pub(crate) plan_digest: Hash,
    /// CBOR key 3: `u32`.
    pub(crate) modes: u32,
    /// CBOR key 4: `u32`.
    pub(crate) parts_done: u32,
    /// CBOR key 5: `VerificationCursor`.
    pub(crate) next: VerificationCursor,
    /// CBOR key 6: `Hash`.
    pub(crate) completed_digest: Hash,
}
/// VerificationVerdict fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationVerdict {
    /// CBOR key 0: `Id`.
    pub operation: Id,
    /// CBOR key 1: native reference, absent when none existed at recording time.
    pub at: Option<TapeAt>,
    /// CBOR key 2: `Bounded<DamagedRange, 4096>`.
    pub damaged: Bounded<DamagedRange, 4096>,
    /// CBOR key 3: `Option<Unprotected>`.
    pub unprotected: Option<Unprotected>,
    /// CBOR key 4: `u32`.
    pub modes: u32,
    /// CBOR key 5: `Option<Hash>`.
    pub edition_digest: Option<Hash>,
    /// CBOR key 6: `Option<DecisionRef>`.
    pub progress: Option<DecisionRef>,
    /// CBOR key 7: verdict subject, including when no native reference exists.
    pub tape: TapeRef,
    /// CBOR key 8: terminal verification outcome.
    pub outcome: VerificationOutcome,
}
/// ManualFinalizationAccepted fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManualFinalizationAccepted {
    /// CBOR key 0: `TapeAt`.
    pub at: TapeAt,
    /// CBOR key 1: `Id`.
    pub operation: Id,
    /// CBOR key 2: `Text`.
    pub actor_fingerprint: Text,
    /// CBOR key 3: `Id`.
    pub idempotency_key: Id,
    /// CBOR key 4: `Hash`.
    pub request_fingerprint: Hash,
    /// CBOR key 5: `u64`.
    pub assignment_generation: u64,
}
/// ConfigurationAccepted fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigurationAccepted {
    /// CBOR key 0: `u64`.
    pub generation: u64,
    /// CBOR key 1: `Hash`.
    pub config_digest: Hash,
    /// CBOR key 2: `Definitions`.
    pub definitions: Definitions,
}
/// Admission scope of a readiness observation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReadinessScope {
    /// Wire code 0: the observed endpoint (ordinary readiness).
    #[default]
    Endpoint,
    /// Wire code 1: every endpoint of the observed library.
    Library,
}
/// Replacement evidence for an already raised alarm; acknowledgement is unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlarmRefresh {
    /// CBOR key 0: replacement severity.
    pub severity: Severity,
    /// CBOR key 1: replacement reason.
    pub reason: Text,
    /// CBOR key 2: replacement causal reference.
    pub cause: Option<DecisionRef>,
}
/// ReadinessObserved fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadinessObserved {
    /// CBOR key 0: `Id`.
    pub operation: Id,
    /// CBOR key 1: `DriveKey`.
    pub endpoint: DriveKey,
    /// CBOR key 2: `Option<TapeRef>`.
    pub tape: Option<TapeRef>,
    /// CBOR key 3: `Option<LabelText>`.
    pub barcode: Option<LabelText>,
    /// CBOR key 4: `ReadinessPhase`.
    pub phase: ReadinessPhase,
    /// CBOR key 5: `ReadinessState`.
    pub state: ReadinessState,
    /// CBOR key 6: `Option<DecisionRef>`.
    pub previous: Option<DecisionRef>,
    /// CBOR key 7: `u64`.
    pub started_unix_ns: u64,
    /// CBOR key 8: deadline duration, or no deadline.
    pub limit_ms: Option<u64>,
    /// CBOR key 9: `Option<CompletionEvidence>`.
    pub evidence: Option<CompletionEvidence>,
    /// CBOR key 10: `Option<DecisionRef>`.
    pub load_intent: Option<DecisionRef>,
    /// CBOR key 11: `u64`.
    pub ready_generation: u64,
    /// CBOR key 12: `ReadinessResult`.
    pub result: ReadinessResult,
    /// CBOR key 13: admission uncertainty scope; ordinary observations use Endpoint.
    pub scope: ReadinessScope,
    /// CBOR key 14: family before provisioning; forbidden with a tape identity.
    pub media: Option<MediumFamily>,
    /// CBOR key 15: optional source slot.
    pub source_slot: Option<u16>,
    /// CBOR key 16: optional run identity.
    pub run: Option<Id>,
    /// CBOR key 17: optional cancellation source.
    pub cancel_source: Option<CancelSource>,
}
/// DriveObserved fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveObserved {
    /// CBOR key 0: `Id`.
    pub drive_uuid: Id,
    /// CBOR key 1: `Option<Text>`.
    pub serial: Option<Text>,
    /// CBOR key 2: `IdentitySource`.
    pub identity_source: IdentitySource,
    /// CBOR key 3: `Option<DriveKey>`.
    pub endpoint: Option<DriveKey>,
    /// CBOR key 4: `Managed`.
    pub managed: Managed,
    /// CBOR key 5: `u64`.
    pub inventory_generation: u64,
    /// CBOR key 6: `Option<Text>`.
    pub vendor: Option<Text>,
    /// CBOR key 7: `Option<Text>`.
    pub product: Option<Text>,
    /// CBOR key 8: `Option<Text>`.
    pub firmware_rev: Option<Text>,
}
/// DriveDecision fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveDecision {
    /// CBOR key 0: `Id`.
    pub drive_uuid: Id,
    /// CBOR key 1: `Stewardship`.
    pub decision: Stewardship,
    /// CBOR key 2: `GenerationChange`.
    pub change: GenerationChange,
}
/// DriveHealthObserved fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveHealthObserved {
    /// CBOR key 0: `Id`.
    pub drive_uuid: Id,
    /// CBOR key 1: `Option<TapeRef>`.
    pub tape: Option<TapeRef>,
    /// CBOR key 2: `Option<Id>`.
    pub session_id: Option<Id>,
    /// CBOR key 3: `Option<Id>`.
    pub operation: Option<Id>,
    /// CBOR key 4: `HealthTrigger`.
    pub trigger: HealthTrigger,
    /// CBOR key 5: `u64`.
    pub tape_alert_flags: u64,
    /// CBOR key 6: `Counters`.
    pub counters: Counters,
    /// CBOR key 7: `Option<Blob<65536>>`.
    pub raw_pages: Option<Blob<65536>>,
}
/// CleaningRecorded fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleaningRecorded {
    /// CBOR key 0: `Text`.
    pub run_id: Text,
    /// CBOR key 1: `Option<DecisionRef>`.
    pub previous: Option<DecisionRef>,
    /// CBOR key 2: `Option<Id>`.
    pub drive_uuid: Option<Id>,
    /// CBOR key 3: `Option<TapeRef>`.
    pub cartridge: Option<TapeRef>,
    /// CBOR key 4: `CleaningAction`.
    pub action: CleaningAction,
}
/// AlarmChanged fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlarmChanged {
    /// CBOR key 0: `Text`.
    pub condition_key: Text,
    /// CBOR key 1: `GenerationChange`.
    pub change: GenerationChange,
    /// CBOR key 2: `AlarmAction`.
    pub action: AlarmAction,
    /// CBOR key 3: `Option<DecisionRef>`.
    pub cause: Option<DecisionRef>,
}
/// OperationRecorded fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationRecorded {
    /// CBOR key 0: `Id`.
    pub operation: Id,
    /// CBOR key 1: `OperationKind`.
    pub kind: OperationKind,
    /// CBOR key 2: `Option<DecisionRef>`.
    pub previous: Option<DecisionRef>,
    /// CBOR key 3: `OperationEvent`.
    pub event: OperationEvent,
}
/// SessionObserved fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionObserved {
    /// CBOR key 0: `Id`.
    pub session_id: Id,
    /// CBOR key 1: `SessionKind`.
    pub kind: SessionKind,
    /// CBOR key 2: `Option<DriveKey>`.
    pub endpoint: Option<DriveKey>,
    /// CBOR key 3: `Option<TapeRef>`.
    pub tape: Option<TapeRef>,
    /// CBOR key 4: `SessionEvent`.
    pub event: SessionEvent,
    /// CBOR key 5: `Option<DecisionRef>`.
    pub previous: Option<DecisionRef>,
    /// CBOR key 6: `Option<Text>`.
    pub reason: Option<Text>,
}
/// WrapMapCached fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrapMapCached {
    /// CBOR key 0: `TapeRef`.
    pub tape: TapeRef,
    /// CBOR key 1: `u64`.
    pub calibration_generation: u64,
    /// CBOR key 2: `WrapAction`.
    pub action: WrapAction,
}
/// RecoveryRecorded fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryRecorded {
    /// CBOR key 0: `TapeAt`.
    pub at: TapeAt,
    /// CBOR key 1: `RecoveryAction`.
    pub action: RecoveryAction,
    /// CBOR key 2: `Id`.
    pub operation: Id,
    /// CBOR key 3: `Option<DecisionRef>`.
    pub prior: Option<DecisionRef>,
}
/// Diagnostic fields in ascending CBOR-key order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// CBOR key 0: `DiagnosticEvent`.
    pub event: DiagnosticEvent,
    /// CBOR key 1: `Text`.
    pub subject_kind: Text,
    /// CBOR key 2: `Option<Text>`.
    pub subject_id: Option<Text>,
    /// CBOR key 3: `Bounded<DiagnosticEntry, 256>`.
    pub detail: Bounded<DiagnosticEntry, 256>,
}
/// Pool assignment source, encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PoolSource {
    /// Wire code 0: operator assignment.
    Operator(Unit),
    /// Wire code 1: latest retained configuration.
    Rule(DecisionRef),
}
/// Closed CancelSource wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum CancelSource {
    /// Wire code 0.
    Operator = 0,
    /// Wire code 1.
    Shutdown = 1,
    /// Wire code 2.
    StartupReconcile = 2,
}
/// Closed VerificationOutcome wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum VerificationOutcome {
    /// Wire code 0.
    VerifiedComplete = 0,
    /// Wire code 1.
    VerifiedDegraded = 1,
    /// Wire code 2.
    RecoveryRequired = 2,
}
/// Closed AssignmentSource wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum AssignmentSource {
    /// Wire code 0.
    Operator = 0,
    /// Wire code 1.
    Rule = 1,
}
/// Closed LabelProvenance wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum LabelProvenance {
    /// Wire code 0.
    Changer = 0,
    /// Wire code 1.
    Operator = 1,
}
/// Closed RetireReason wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum RetireReason {
    /// Wire code 0.
    OperatorRequested = 0,
}
/// Closed ProvisionMode wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum ProvisionMode {
    /// Wire code 0.
    Initialize = 0,
    /// Wire code 1.
    Adopt = 1,
}
/// Closed Dispatch wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Dispatch {
    /// Wire code 0.
    NotIssued = 0,
    /// Wire code 1.
    Issued = 1,
}
/// Closed Completion wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Completion {
    /// Wire code 0.
    Known = 0,
    /// Wire code 1.
    Unknown = 1,
}
/// Closed FenceKind wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum FenceKind {
    /// Wire code 0.
    Write = 0,
    /// Wire code 1.
    DriveFault = 1,
}
/// Closed FenceCause wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum FenceCause {
    /// Wire code 0; producer vocabulary is recorded in SCHEMA.md.
    PartialBatch = 0,
    /// Wire code 1; producer vocabulary is recorded in SCHEMA.md.
    TransferError = 1,
    /// Wire code 2; producer vocabulary is recorded in SCHEMA.md.
    CloseoutCheckMismatch = 2,
    /// Wire code 3; producer vocabulary is recorded in SCHEMA.md.
    SupersedeTailFailed = 3,
    /// Wire code 4; producer vocabulary is recorded in SCHEMA.md.
    StandaloneWriteLostByRestart = 4,
    /// Wire code 5; producer vocabulary is recorded in SCHEMA.md.
    AppendAdmissionRefused = 5,
    /// Wire code 6; producer vocabulary is recorded in SCHEMA.md.
    ResetUnitAttention = 6,
    /// Wire code 7; producer vocabulary is recorded in SCHEMA.md.
    PositionDrift = 7,
    /// Wire code 8; producer vocabulary is recorded in SCHEMA.md.
    AuthorityAmbiguous = 8,
    /// Wire code 9; producer vocabulary is recorded in SCHEMA.md.
    SessionSuffixContradiction = 9,
    /// Wire code 10; producer vocabulary is recorded in SCHEMA.md.
    CloseoutReadContradiction = 10,
    /// Wire code 11; producer vocabulary is recorded in SCHEMA.md.
    Cleaning = 11,
    /// Wire code 12; decoded but reserved until disk quarantine is implemented.
    ContainerQuarantine = 12,
    /// Wire code 13; a checkpoint durability barrier failed.
    CheckpointBarrierFailed = 13,
    /// Wire code 14.
    NativeLogBehindProjection = 14,
}
/// Closed MotionResult wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum MotionResult {
    /// Wire code 0.
    Done = 0,
    /// Wire code 1.
    Failed = 1,
    /// Wire code 2.
    Unknown = 2,
}
/// Closed ElementKind wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum ElementKind {
    /// Wire code 0.
    Transport = 0,
    /// Wire code 1.
    Storage = 1,
    /// Wire code 2.
    ImportExport = 2,
    /// Wire code 3.
    Drive = 3,
}
/// Closed SelectionPolicy wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SelectionPolicy {
    /// Wire code 0.
    CompleteOrFill = 0,
    /// Wire code 1.
    FillOldest = 1,
}
/// Closed PhysicalTail wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum PhysicalTail {
    /// Wire code 0.
    ExactBootstrapFilemarkEod = 0,
    /// Wire code 1.
    DataAfterBootstrap = 1,
    /// Wire code 2.
    MissingFilemark = 2,
    /// Wire code 3.
    ExtraFilemark = 3,
    /// Wire code 4.
    Ambiguous = 4,
}
/// Closed AdoptedState wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum AdoptedState {
    /// Wire code 0.
    Ready = 0,
    /// Wire code 1.
    RecoveryRequired = 1,
}
/// Closed ReadinessPhase wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum ReadinessPhase {
    /// Wire code 0; production readiness phase.
    Planned = 0,
    /// Wire code 1; production readiness phase.
    InitializingCommandLoad = 1,
    /// Wire code 2; production readiness phase.
    ReadinessPoll = 2,
    /// Wire code 3; production readiness phase.
    Ready = 3,
    /// Wire code 4; production readiness phase.
    Cancelled = 4,
    /// Wire code 5; production readiness phase.
    Shutdown = 5,
    /// Wire code 6; production readiness phase.
    SessionOpenShortProbe = 6,
    /// Wire code 7; production readiness phase.
    SessionOpenImmediateLoad = 7,
    /// Wire code 8; production readiness phase.
    SessionOpenAfterImmediateLoad = 8,
    /// Wire code 9; production readiness phase.
    StartupReconcileTur = 9,
    /// Wire code 10; production readiness phase.
    StartupReconcile = 10,
    /// Wire code 11; production readiness phase.
    StartupReconcileRequiresRelease = 11,
    /// Wire code 12; production readiness phase.
    QuarantineRelease = 12,
    /// Wire code 13; production readiness phase.
    Released = 13,
    /// Wire code 14; production readiness phase.
    WaitReadyTur = 14,
    /// Wire code 15; production readiness phase.
    AfterWaitReadyTur = 15,
    /// Wire code 16; production readiness phase.
    BeforeConditionalImmediateLoad = 16,
    /// Wire code 17; production readiness phase.
    ConditionalImmediateLoad = 17,
    /// Wire code 18; production readiness phase.
    AfterConditionalImmediateLoad = 18,
    /// Wire code 19; production readiness phase.
    AfterBotClassification = 19,
    /// Wire code 20; production readiness phase.
    AfterClobberConfirmation = 20,
    /// Wire code 21; production readiness phase.
    AfterMoveMedium = 21,
    /// Wire code 22; production readiness phase.
    AfterOpenDrive = 22,
    /// Wire code 23; production readiness phase.
    AfterOpenLibrary = 23,
    /// Wire code 24; production readiness phase.
    AfterReadMediumPartitions = 24,
    /// Wire code 25; production readiness phase.
    AfterRewindBeforeBotRead = 25,
    /// Wire code 26; production readiness phase.
    ApplyInitWriteGate = 26,
    /// Wire code 27; production readiness phase.
    BeforeApplyInitWriteGate = 27,
    /// Wire code 28; production readiness phase.
    BeforeMoveMedium = 28,
    /// Wire code 29; production readiness phase.
    BeforeOpenDrive = 29,
    /// Wire code 30; production readiness phase.
    BeforeOpenLibrary = 30,
    /// Wire code 31; production readiness phase.
    BeforeReadDriveConfig = 31,
    /// Wire code 32; production readiness phase.
    BeforeRewindBeforeBootstrapWrite = 32,
    /// Wire code 33; production readiness phase.
    BeforeRewindBeforeBotRead = 33,
    /// Wire code 34; production readiness phase.
    BeforeWaitReadyTur = 34,
    /// Wire code 35; production readiness phase.
    MoveMedium = 35,
    /// Wire code 36; production readiness phase.
    OpenDrive = 36,
    /// Wire code 37; production readiness phase.
    OpenLibrary = 37,
    /// Wire code 38; production readiness phase.
    ReadDriveConfig = 38,
    /// Wire code 39; production readiness phase.
    ReadMediumPartitions = 39,
    /// Wire code 40; production readiness phase.
    RewindBeforeBootstrapWrite = 40,
    /// Wire code 41; production readiness phase.
    RewindBeforeBotRead = 41,
    /// Wire code 42; production readiness phase.
    ClobberConfirmation = 42,
    /// Wire code 43; production readiness phase.
    PreLoadTur = 43,
    /// Wire code 44; production readiness phase.
    AlreadyLoadedTur = 44,
    /// Wire code 45; production readiness phase.
    BeforePreLoadTur = 45,
    /// Wire code 46; production readiness phase.
    AfterPreLoadTur = 46,
    /// Wire code 47; production readiness phase.
    BeforeAlreadyLoadedTur = 47,
    /// Wire code 48; production readiness phase.
    AfterAlreadyLoadedTur = 48,
    /// Wire code 49.
    LoadDriveReadiness = 49,
    /// Wire code 50.
    LoadDriveCompletion = 50,
}
/// Closed ReadinessState wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum ReadinessState {
    /// Wire code 0.
    Planned = 0,
    /// Wire code 1.
    PreReadyLoading = 1,
    /// Wire code 2.
    Ready = 2,
    /// Wire code 3.
    MediaInitializing = 3,
    /// Wire code 4.
    BecomingReady = 4,
    /// Wire code 5.
    UnitAttention = 5,
    /// Wire code 6.
    TargetBusy = 6,
    /// Wire code 7.
    ReservationConflict = 7,
    /// Wire code 8.
    TransportUnknown = 8,
    /// Wire code 9.
    TerminalError = 9,
    /// Wire code 10.
    TimeoutUnknown = 10,
    /// Wire code 11.
    AbortedUnknown = 11,
    /// Wire code 12.
    Cancelled = 12,
    /// Wire code 13.
    Released = 13,
    /// Wire code 14.
    Starting = 14,
    /// Wire code 15.
    RecordingFailed = 15,
}
/// Closed ReadinessResult wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum ReadinessResult {
    /// Wire code 0.
    Pending = 0,
    /// Wire code 1.
    Ready = 1,
    /// Wire code 2.
    Failed = 2,
    /// Wire code 3.
    Cancelled = 3,
    /// Wire code 4.
    AbortedUnknown = 4,
    /// Wire code 5.
    Released = 5,
}
/// Closed IdentitySource wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum IdentitySource {
    /// Wire code 0.
    Standalone = 0,
    /// Wire code 1.
    DvcidInline = 1,
    /// Wire code 2.
    DvcidAndInquiry = 2,
    /// Wire code 3.
    Derived = 3,
}
/// Closed Managed wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Managed {
    /// Wire code 0.
    Rem = 0,
    /// Wire code 1.
    Foreign = 1,
}
/// Closed HealthTrigger wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum HealthTrigger {
    /// Wire code 0; producer vocabulary is recorded in SCHEMA.md.
    SessionClose = 0,
    /// Wire code 1; producer vocabulary is recorded in SCHEMA.md.
    AppendFailure = 1,
    /// Wire code 2; producer vocabulary is recorded in SCHEMA.md.
    ReadFailure = 2,
    /// Wire code 3; producer vocabulary is recorded in SCHEMA.md.
    ForeignCounter = 3,
    /// Wire code 4; producer vocabulary is recorded in SCHEMA.md.
    Manual = 4,
}
/// Closed CleanTrigger wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum CleanTrigger {
    /// Wire code 0; producer vocabulary is recorded in SCHEMA.md.
    Manual = 0,
    /// Wire code 1; producer vocabulary is recorded in SCHEMA.md.
    Periodic = 1,
    /// Wire code 2; producer vocabulary is recorded in SCHEMA.md.
    Now = 2,
}
/// Closed CleanPhase wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum CleanPhase {
    /// Wire code 0; producer vocabulary is recorded in SCHEMA.md.
    Fencing = 0,
    /// Wire code 1; producer vocabulary is recorded in SCHEMA.md.
    Selecting = 1,
    /// Wire code 2; producer vocabulary is recorded in SCHEMA.md.
    MovingIn = 2,
    /// Wire code 3; producer vocabulary is recorded in SCHEMA.md.
    Cleaning = 3,
    /// Wire code 4; producer vocabulary is recorded in SCHEMA.md.
    MovingBack = 4,
    /// Wire code 5; producer vocabulary is recorded in SCHEMA.md.
    Verifying = 5,
    /// Wire code 6; producer vocabulary is recorded in SCHEMA.md.
    Done = 6,
    /// Wire code 7; producer vocabulary is recorded in SCHEMA.md.
    Failed = 7,
    /// Wire code 8; producer vocabulary is recorded in SCHEMA.md.
    NeedsOperator = 8,
}
/// Closed CleanResult wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum CleanResult {
    /// Wire code 0.
    Done = 0,
    /// Wire code 1.
    Failed = 1,
    /// Wire code 2.
    NeedsOperator = 2,
}
/// Closed AlarmKind wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum AlarmKind {
    /// Wire code 0; producer vocabulary is recorded in SCHEMA.md.
    DriveCleaningAbnormalFrequency = 0,
    /// Wire code 1; producer vocabulary is recorded in SCHEMA.md.
    CleaningNeedsOperator = 1,
    /// Wire code 2; producer vocabulary is recorded in SCHEMA.md.
    NoClnCart = 2,
    /// Wire code 3; producer vocabulary is recorded in SCHEMA.md.
    ClnCartExpired = 3,
    /// Wire code 4; producer vocabulary is recorded in SCHEMA.md.
    CartNotCleaningBehavior = 4,
    /// Wire code 5; producer vocabulary is recorded in SCHEMA.md.
    SnapshotPersistFailing = 5,
    /// Wire code 6; producer vocabulary is recorded in SCHEMA.md.
    CheckpointBarrierOverdue = 6,
    /// Wire code 7; producer vocabulary is recorded in SCHEMA.md.
    ForeignDriveWantsCleaning = 7,
    /// Wire code 8; producer vocabulary is recorded in SCHEMA.md.
    KindFlipRefused = 8,
    /// Wire code 9; native evidence contradicts a library reference.
    ContradictedTape = 9,
    /// Wire code 10; a durable pending record failed resolution.
    PendingRecordRejected = 10,
    /// Wire code 11.
    DriveSerialCollision = 11,
    /// Wire code 12.
    RetiredDriveReappeared = 12,
}
/// Closed Severity wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Severity {
    /// Wire code 0; producer vocabulary is recorded in SCHEMA.md.
    Warning = 0,
    /// Wire code 1; producer vocabulary is recorded in SCHEMA.md.
    Critical = 1,
    /// Wire code 2; producer vocabulary is recorded in SCHEMA.md.
    Error = 2,
}
/// Closed OperationKind wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum OperationKind {
    /// Wire code 0; producer vocabulary is recorded in SCHEMA.md.
    Unknown = 0,
    /// Wire code 1; producer vocabulary is recorded in SCHEMA.md.
    AckAlarm = 1,
    /// Wire code 2; producer vocabulary is recorded in SCHEMA.md.
    DriveStewardship = 2,
    /// Wire code 3; producer vocabulary is recorded in SCHEMA.md.
    CleanDrive = 3,
    /// Wire code 4; producer vocabulary is recorded in SCHEMA.md.
    RefreshInventory = 4,
    /// Wire code 5; producer vocabulary is recorded in SCHEMA.md.
    MoveMedium = 5,
    /// Wire code 6; producer vocabulary is recorded in SCHEMA.md.
    LoadDrive = 6,
    /// Wire code 7; producer vocabulary is recorded in SCHEMA.md.
    UnloadDrive = 7,
    /// Wire code 8; producer vocabulary is recorded in SCHEMA.md.
    MediaReadinessWait = 8,
    /// Wire code 9; producer vocabulary is recorded in SCHEMA.md.
    TapeVerification = 9,
    /// Wire code 10; producer vocabulary is recorded in SCHEMA.md.
    FinalizeTape = 10,
    /// Wire code 11; producer vocabulary is recorded in SCHEMA.md.
    ReconcileTape = 11,
    /// Wire code 12; producer vocabulary is recorded in SCHEMA.md.
    TapeCloseout = 12,
    /// Wire code 13; producer vocabulary is recorded in SCHEMA.md.
    CloseoutOverdue = 13,
}
/// Closed Retry wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Retry {
    /// Wire code 0.
    Transient = 0,
    /// Wire code 1.
    Operator = 1,
    /// Wire code 2.
    Media = 2,
}
/// Closed SessionKind wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SessionKind {
    /// Wire code 1.
    Read = 1,
    /// Wire code 2.
    Operation = 2,
}
/// Closed SessionEvent wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SessionEvent {
    /// Wire code 0.
    Opened = 0,
    /// Wire code 1.
    Closed = 1,
    /// Wire code 2.
    Orphaned = 2,
    /// Wire code 3.
    LostByRestart = 3,
}
/// Closed DiagnosticEvent wire vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum DiagnosticEvent {
    /// Wire code 0.
    RequestReceived = 0,
    /// Wire code 1.
    OperationStarted = 1,
    /// Wire code 2.
    OperationProgress = 2,
    /// Wire code 3.
    OperationFinished = 3,
    /// Wire code 4.
    OperationFailed = 4,
    /// Wire code 5.
    CancelRequested = 5,
    /// Wire code 6.
    CancelledBeforeDispatch = 6,
    /// Wire code 7.
    CompletedAfterCancel = 7,
    /// Wire code 8.
    CancellationRejected = 8,
    /// Wire code 9.
    CompletionUnknown = 9,
    /// Wire code 10.
    ClockRegressionObserved = 10,
    /// Wire code 11.
    ClockForwardJumpObserved = 11,
    /// Wire code 12.
    HardwareWarning = 12,
    /// Wire code 13.
    RecoveryEvent = 13,
    /// Wire code 14.
    ConfigLoaded = 14,
    /// Wire code 15.
    ConfigRejected = 15,
    /// Wire code 16.
    IndexRebuilt = 16,
    /// Wire code 17.
    ReadOnlyModeEntered = 17,
    /// Wire code 18.
    ReadOnlyModeLeft = 18,
    /// Wire code 19.
    AuditWriteFailed = 19,
    /// Wire code 20.
    RefusalRecorded = 20,
}
/// DriveKey: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DriveKey {
    /// Wire code 0.
    Bay(Bay),
    /// Wire code 1.
    Standalone(Standalone),
}
/// Inventory observation subject: a changer or one standalone endpoint.
/// Standalone observations are obtained with TEST UNIT READY and READ POSITION;
/// they never borrow a changer identity or resolve another endpoint's intents.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum InventorySubject {
    /// Wire code 0: library serial, observed through element status.
    Library(Text),
    /// Wire code 1: standalone endpoint, observed through TUR and READ POSITION.
    Standalone(Standalone),
}

/// FenceScope: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FenceScope {
    /// Wire code 0.
    Tape(TapeRef),
    /// Wire code 1.
    Drive(DriveKey),
    /// Wire code 2; decoded but reserved until model qualification is implemented.
    Model {
        /// Hardware model.
        model: Text,
        /// Firmware revision.
        firmware: Text,
    },
}
/// Motion: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Motion {
    /// Wire code 0.
    Load(Load),
    /// Wire code 1.
    Unload(Unload),
    /// Wire code 2.
    Move(Move),
}
/// Confirmation: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Confirmation {
    /// Wire code 0.
    Inventory(u64),
    /// Wire code 1.
    Readiness(u64),
}
/// Stewardship: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stewardship {
    /// Wire code 0.
    Retire(Reason),
    /// Wire code 1.
    Reinstate(Reason),
    /// Wire code 2.
    Annotate(Annotation),
}
/// CleaningAction: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleaningAction {
    /// Wire code 0.
    Register(Register),
    /// Wire code 1.
    Begin(Begin),
    /// Wire code 2.
    Advance(Advance),
    /// Wire code 3.
    Finish(Finish),
    /// Wire code 4.
    Expire(Reason),
    /// Wire code 5. Permanently reject an unexpired registered cartridge.
    Reject(Reason),
}
/// AlarmAction: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AlarmAction {
    /// Wire code 0.
    Raise(AlarmRaise),
    /// Wire code 1.
    Acknowledge(Unit),
    /// Wire code 2.
    Clear(Reason),
    /// Wire code 3: update evidence without clearing acknowledgement.
    Refresh(AlarmRefresh),
}
/// OperationEvent: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationEvent {
    /// Wire code 0.
    Accepted(Accepted),
    /// Wire code 1.
    Started(Unit),
    /// Wire code 2.
    CancelRequested(Unit),
    /// Wire code 3.
    CancellationRejected(Unit),
    /// Wire code 4.
    CancelledBeforeDispatch(Unit),
    /// Wire code 5.
    CompletedAfterCancel(Response),
    /// Wire code 6.
    Finished(Response),
    /// Wire code 7.
    Failed(Failed),
    /// Wire code 8.
    CompletionUnknown(Unit),
}
/// WrapAction: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WrapAction {
    /// Wire code 0.
    Cache(Cache),
    /// Wire code 1.
    Invalidate(Reason),
}
/// RecoveryAction: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Wire code 1.
    RepairApplied(RepairApplied),
    /// Wire code 2.
    AppendAuthorityRecovered(Recovered),
}
/// DiagnosticValue: each variant is encoded as `[code, payload]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiagnosticValue {
    /// Wire code 0.
    Text(Text),
    /// Wire code 1.
    Unsigned(u64),
    /// Wire code 2.
    Negative(Negative),
    /// Wire code 3.
    Bytes(Blob<4194304>),
    /// Wire code 4.
    Bool(bool),
    /// Wire code 5.
    Null(Unit),
}
/// One diagnostic key and scalar value; keys must be strictly ascending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticEntry(pub Text, pub DiagnosticValue);
/// Decoded structural value; sequence is exclusively in the frame prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Attribution.
    pub envelope: Envelope,
    /// Typed authority or diagnostic.
    body: Body,
}
/// Allocated record kinds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum Tag {
    /// AssignPool.
    AssignPool = 1,
    /// Label.
    Label = 2,
    /// Retire.
    Retire = 3,
    /// Reprovision.
    Reprovision = 4,
    /// MediumSpecSet.
    MediumSpecSet = 5,
    /// SupersedeTail.
    SupersedeTail = 6,
    /// AcceptUnprotectedFinalization.
    AcceptUnprotectedFinalization = 7,
    /// FenceRaised.
    FenceRaised = 8,
    /// FenceReleased.
    FenceReleased = 9,
    /// MotionIntent.
    MotionIntent = 10,
    /// MotionOutcome.
    MotionOutcome = 11,
    /// InventoryReconciled.
    InventoryReconciled = 12,
    /// VerificationProgress.
    VerificationProgress = 13,
    /// VerificationVerdict.
    VerificationVerdict = 14,
    /// ManualFinalizationAccepted.
    ManualFinalizationAccepted = 15,
    /// ConfigurationAccepted.
    ConfigurationAccepted = 16,
    /// ReadinessObserved.
    ReadinessObserved = 17,
    /// DriveObserved.
    DriveObserved = 18,
    /// DriveDecision.
    DriveDecision = 19,
    /// DriveHealthObserved.
    DriveHealthObserved = 20,
    /// CleaningRecorded.
    CleaningRecorded = 21,
    /// AlarmChanged.
    AlarmChanged = 22,
    /// OperationRecorded.
    OperationRecorded = 23,
    /// SessionObserved.
    SessionObserved = 24,
    /// WrapMapCached.
    WrapMapCached = 25,
    /// RecoveryRecorded.
    RecoveryRecorded = 26,
    /// Diagnostic.
    Diagnostic = 32,
}
/// Body selected by the prefix tag. Construction is confined to this crate's
/// library writer and codec; downstream code uses LibraryAuthority.
///
/// ```compile_fail
/// use remanence_state::library_log::{Body, Diagnostic};
/// fn construct(value: Diagnostic) { let _ = Body::Diagnostic(value); }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
enum Body {
    /// AssignPool.
    AssignPool(AssignPool),
    /// Label.
    Label(Label),
    /// Retire.
    Retire(Retire),
    /// Reprovision.
    Reprovision(Reprovision),
    /// MediumSpecSet.
    MediumSpecSet(MediumSpecSet),
    /// SupersedeTail.
    SupersedeTail(SupersedeTail),
    /// AcceptUnprotectedFinalization.
    AcceptUnprotectedFinalization(AcceptUnprotectedFinalization),
    /// FenceRaised.
    FenceRaised(FenceRaised),
    /// FenceReleased.
    FenceReleased(FenceReleased),
    /// MotionIntent.
    MotionIntent(MotionIntent),
    /// MotionOutcome.
    MotionOutcome(MotionOutcome),
    /// InventoryReconciled.
    InventoryReconciled(InventoryReconciled),
    /// VerificationProgress.
    VerificationProgress(VerificationProgress),
    /// VerificationVerdict.
    VerificationVerdict(VerificationVerdict),
    /// ManualFinalizationAccepted.
    ManualFinalizationAccepted(ManualFinalizationAccepted),
    /// ConfigurationAccepted.
    ConfigurationAccepted(ConfigurationAccepted),
    /// ReadinessObserved.
    ReadinessObserved(ReadinessObserved),
    /// DriveObserved.
    DriveObserved(DriveObserved),
    /// DriveDecision.
    DriveDecision(DriveDecision),
    /// DriveHealthObserved.
    DriveHealthObserved(DriveHealthObserved),
    /// CleaningRecorded.
    CleaningRecorded(CleaningRecorded),
    /// AlarmChanged.
    AlarmChanged(AlarmChanged),
    /// OperationRecorded.
    OperationRecorded(OperationRecorded),
    /// SessionObserved.
    SessionObserved(SessionObserved),
    /// WrapMapCached.
    WrapMapCached(WrapMapCached),
    /// RecoveryRecorded.
    RecoveryRecorded(RecoveryRecorded),
    /// Diagnostic.
    Diagnostic(Diagnostic),
}
impl Body {
    /// The allocated frame tag.
    pub fn tag(&self) -> Tag {
        match self {
            Self::AssignPool(..) => Tag::AssignPool,
            Self::Label(..) => Tag::Label,
            Self::Retire(..) => Tag::Retire,
            Self::Reprovision(..) => Tag::Reprovision,
            Self::MediumSpecSet(..) => Tag::MediumSpecSet,
            Self::SupersedeTail(..) => Tag::SupersedeTail,
            Self::AcceptUnprotectedFinalization(..) => Tag::AcceptUnprotectedFinalization,
            Self::FenceRaised(..) => Tag::FenceRaised,
            Self::FenceReleased(..) => Tag::FenceReleased,
            Self::MotionIntent(..) => Tag::MotionIntent,
            Self::MotionOutcome(..) => Tag::MotionOutcome,
            Self::InventoryReconciled(..) => Tag::InventoryReconciled,
            Self::VerificationProgress(..) => Tag::VerificationProgress,
            Self::VerificationVerdict(..) => Tag::VerificationVerdict,
            Self::ManualFinalizationAccepted(..) => Tag::ManualFinalizationAccepted,
            Self::ConfigurationAccepted(..) => Tag::ConfigurationAccepted,
            Self::ReadinessObserved(..) => Tag::ReadinessObserved,
            Self::DriveObserved(..) => Tag::DriveObserved,
            Self::DriveDecision(..) => Tag::DriveDecision,
            Self::DriveHealthObserved(..) => Tag::DriveHealthObserved,
            Self::CleaningRecorded(..) => Tag::CleaningRecorded,
            Self::AlarmChanged(..) => Tag::AlarmChanged,
            Self::OperationRecorded(..) => Tag::OperationRecorded,
            Self::SessionObserved(..) => Tag::SessionObserved,
            Self::WrapMapCached(..) => Tag::WrapMapCached,
            Self::RecoveryRecorded(..) => Tag::RecoveryRecorded,
            Self::Diagnostic(..) => Tag::Diagnostic,
        }
    }
}

/// Stable fence identity within one library authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FenceId {
    /// Manifest log UUID.
    pub log_uuid: Id,
    /// Sequence of the raise.
    pub sequence: u64,
}
impl std::fmt::Display for FenceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "fence:{}:{}",
            uuid::Uuid::from_bytes(self.log_uuid),
            self.sequence
        )
    }
}
/// Construct the reserved unprotected decision only for fixtures and codec tests.
#[cfg(any(test, feature = "fixtures"))]
pub fn fixture_unprotected(
    at: TapeAt,
    operation: Id,
    w: u64,
    t: u64,
    reason: Text,
) -> AcceptUnprotectedFinalization {
    AcceptUnprotectedFinalization {
        at,
        operation,
        w,
        t,
        reason,
    }
}
/// Construct step-7 verification progress without connecting a production emitter.
#[cfg(any(test, feature = "fixtures"))]
pub fn fixture_progress(
    operation: Id,
    at: TapeAt,
    plan_digest: Hash,
    modes: u32,
    parts_done: u32,
    next: VerificationCursor,
    completed_digest: Hash,
) -> VerificationProgress {
    VerificationProgress {
        operation,
        at,
        plan_digest,
        modes,
        parts_done,
        next,
        completed_digest,
    }
}
impl AcceptUnprotectedFinalization {
    /// Causal operation.
    pub fn operation(&self) -> Id {
        self.operation
    }
    /// Operator presentation, ignored by the fold.
    pub fn reason(&self) -> &str {
        self.reason.as_str()
    }
    /// Native counterpart required by this reserved decision.
    pub fn at(&self) -> &TapeAt {
        &self.at
    }
    /// Accepted half-open range.
    pub fn range(&self) -> (u64, u64) {
        (self.w, self.t)
    }
}
impl VerificationProgress {
    /// Causal operation.
    pub fn operation(&self) -> Id {
        self.operation
    }
    /// Immutable plan identity.
    pub fn plan_digest(&self) -> Hash {
        self.plan_digest
    }
    /// Verification mode bits.
    pub fn modes(&self) -> u32 {
        self.modes
    }
    /// Digest of completed work.
    pub fn completed_digest(&self) -> Hash {
        self.completed_digest
    }
    /// Native coverage for the progress cursor.
    pub fn at(&self) -> &TapeAt {
        &self.at
    }
    /// Number of completed parts.
    pub fn parts_done(&self) -> u32 {
        self.parts_done
    }
    /// Next cursor.
    pub fn next(&self) -> VerificationCursor {
        self.next
    }
}

/// The native parity map with a library-bounded scheme identifier; wire keys match tape ParityScheme.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParityScheme {
    /// Key 0: bounded scheme identifier.
    pub id: Text,
    /// Key 1: data blocks per stripe.
    pub data_blocks_per_stripe: u16,
    /// Key 2: parity blocks per stripe.
    pub parity_blocks_per_stripe: u16,
    /// Key 3: stripes per neighborhood.
    pub stripes_per_neighborhood: u32,
}
impl ParityScheme {
    fn validate(&self) -> Result<(), ()> {
        remanence_parity::ParityScheme {
            id: remanence_parity::SchemeId::new_owned(self.id.0.clone()),
            data_blocks_per_stripe: self.data_blocks_per_stripe,
            parity_blocks_per_stripe: self.parity_blocks_per_stripe,
            stripes_per_neighborhood: self.stripes_per_neighborhood,
        }
        .validate()
        .map(|_| ())
        .map_err(|_| ())
    }
}

#[cfg(test)]
mod library_log_b2_fix;

#[cfg(test)]
mod library_log_b2_fix2;

#[cfg(test)]
mod library_log_memory;

impl Record {
    /// Read the typed alarm body without permitting record construction.
    pub fn alarm_changed(&self) -> Option<&AlarmChanged> {
        match &self.body {
            Body::AlarmChanged(value) => Some(value),
            _ => None,
        }
    }
    /// Read the typed operation body without permitting record construction.
    pub fn operation_recorded(&self) -> Option<&OperationRecorded> {
        match &self.body {
            Body::OperationRecorded(value) => Some(value),
            _ => None,
        }
    }
    /// Read the typed cleaning body without permitting record construction.
    pub fn cleaning_recorded(&self) -> Option<&CleaningRecorded> {
        match &self.body {
            Body::CleaningRecorded(value) => Some(value),
            _ => None,
        }
    }
    /// Borrow the authenticated readiness owner facts and actual observation.
    pub fn readiness_observation(&self) -> Option<&ReadinessObserved> {
        match &self.body {
            Body::ReadinessObserved(value) => Some(value),
            _ => None,
        }
    }
    /// Borrow an authenticated motion intent without exposing body construction.
    pub fn motion_intent(&self) -> Option<&MotionIntent> {
        match &self.body {
            Body::MotionIntent(value) => Some(value),
            _ => None,
        }
    }
    /// Borrow provisioning evidence without exposing body construction.
    pub fn reprovision(&self) -> Option<&Reprovision> {
        match &self.body {
            Body::Reprovision(value) => Some(value),
            _ => None,
        }
    }
    /// Borrow the physical inventory observation named by a motion outcome.
    pub fn inventory_observation(&self) -> Option<&InventoryReconciled> {
        match &self.body {
            Body::InventoryReconciled(value) => Some(value),
            _ => None,
        }
    }
    /// Borrow a motion outcome for read-only status and replay verification.
    pub fn motion_outcome(&self) -> Option<&MotionOutcome> {
        match &self.body {
            Body::MotionOutcome(value) => Some(value),
            _ => None,
        }
    }
    /// Allocated wire tag, without exposing record construction.
    pub fn tag(&self) -> Tag {
        self.body.tag()
    }
    pub(crate) fn canonical_body(&self) -> std::io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        wire::encode_body(&self.body, &mut bytes)?;
        Ok(bytes)
    }
}

/// Borrowed inspection vocabulary. This cannot construct an authority body.
#[derive(Clone, Copy)]
#[allow(dead_code)] // Reserved and tape-owned records have no SQL fields to inspect.
pub(crate) enum BodyView<'a> {
    AssignPool(&'a AssignPool),
    Label(&'a Label),
    Retire(&'a Retire),
    Reprovision(&'a Reprovision),
    MediumSpecSet(&'a MediumSpecSet),
    SupersedeTail(&'a SupersedeTail),
    AcceptUnprotectedFinalization(&'a AcceptUnprotectedFinalization),
    FenceRaised(&'a FenceRaised),
    FenceReleased(&'a FenceReleased),
    MotionIntent(&'a MotionIntent),
    MotionOutcome(&'a MotionOutcome),
    InventoryReconciled(&'a InventoryReconciled),
    VerificationProgress(&'a VerificationProgress),
    VerificationVerdict(&'a VerificationVerdict),
    ManualFinalizationAccepted(&'a ManualFinalizationAccepted),
    ConfigurationAccepted(&'a ConfigurationAccepted),
    ReadinessObserved(&'a ReadinessObserved),
    DriveObserved(&'a DriveObserved),
    DriveDecision(&'a DriveDecision),
    DriveHealthObserved(&'a DriveHealthObserved),
    CleaningRecorded(&'a CleaningRecorded),
    AlarmChanged(&'a AlarmChanged),
    OperationRecorded(&'a OperationRecorded),
    SessionObserved(&'a SessionObserved),
    WrapMapCached(&'a WrapMapCached),
    RecoveryRecorded(&'a RecoveryRecorded),
    Diagnostic(&'a Diagnostic),
}
impl Record {
    pub(crate) fn body_view(&self) -> BodyView<'_> {
        match &self.body {
            Body::AssignPool(value) => BodyView::AssignPool(value),
            Body::Label(value) => BodyView::Label(value),
            Body::Retire(value) => BodyView::Retire(value),
            Body::Reprovision(value) => BodyView::Reprovision(value),
            Body::MediumSpecSet(value) => BodyView::MediumSpecSet(value),
            Body::SupersedeTail(value) => BodyView::SupersedeTail(value),
            Body::AcceptUnprotectedFinalization(value) => {
                BodyView::AcceptUnprotectedFinalization(value)
            }
            Body::FenceRaised(value) => BodyView::FenceRaised(value),
            Body::FenceReleased(value) => BodyView::FenceReleased(value),
            Body::MotionIntent(value) => BodyView::MotionIntent(value),
            Body::MotionOutcome(value) => BodyView::MotionOutcome(value),
            Body::InventoryReconciled(value) => BodyView::InventoryReconciled(value),
            Body::VerificationProgress(value) => BodyView::VerificationProgress(value),
            Body::VerificationVerdict(value) => BodyView::VerificationVerdict(value),
            Body::ManualFinalizationAccepted(value) => BodyView::ManualFinalizationAccepted(value),
            Body::ConfigurationAccepted(value) => BodyView::ConfigurationAccepted(value),
            Body::ReadinessObserved(value) => BodyView::ReadinessObserved(value),
            Body::DriveObserved(value) => BodyView::DriveObserved(value),
            Body::DriveDecision(value) => BodyView::DriveDecision(value),
            Body::DriveHealthObserved(value) => BodyView::DriveHealthObserved(value),
            Body::CleaningRecorded(value) => BodyView::CleaningRecorded(value),
            Body::AlarmChanged(value) => BodyView::AlarmChanged(value),
            Body::OperationRecorded(value) => BodyView::OperationRecorded(value),
            Body::SessionObserved(value) => BodyView::SessionObserved(value),
            Body::WrapMapCached(value) => BodyView::WrapMapCached(value),
            Body::RecoveryRecorded(value) => BodyView::RecoveryRecorded(value),
            Body::Diagnostic(value) => BodyView::Diagnostic(value),
        }
    }
}

/// Native crash acknowledgement join and shared preview encoding.
pub mod session_ack;
