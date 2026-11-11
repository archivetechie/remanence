//! Pure causal validation and resource-scoped replay. Presentation text never selects a rule.
use super::index::{Index, ProbeCounter};
use super::*;
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};
/// A tape counterpart includes its decoded tape record kind, never diagnostic text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TapeLookup {
    /// A complete native counterpart.
    Present {
        /// Tape log's own tag.
        kind: crate::tape_log::Tag,
        /// Digest over prefix, structural and payload bytes.
        digest: Hash,
    },
    /// The immutable header of an existing log, with no invented record kind.
    EmptyPrefix {
        /// SHA-256 of the exact immutable header bytes.
        digest: Hash,
    },
    /// Absent or below the requested closed sequence.
    NotYet,
    /// Identity or content contradiction.
    Contradiction,
}
/// Read-only native proof lookup supplied by the authority owner.
pub trait TapeView {
    /// Look up immutable native evidence. Read failures must fail the operation,
    /// not become an absent proof or a persistent tape contradiction.
    fn lookup(&self, at: &TapeAt) -> Result<TapeLookup, FoldError>;
}
/// Read-only historical records. DecisionRef absence includes a UUID mismatch. Implementations
/// may read storage but must never mutate authority or retain a decoded history.
pub trait LibraryHistory {
    /// Match both sequence and record UUID; decode at most one record at a time.
    /// I/O failure must fail closed.
    fn lookup(&self, reference: DecisionRef) -> Result<Option<Record>, FoldError>;
    /// Look up a sequence in this log, whose UUID is bound by its segment headers.
    /// Only references whose wire form is a FenceId may use this method;
    /// every DecisionRef must use the UUID-matching `lookup` instead.
    fn lookup_fence(&self, sequence: u64) -> Result<Option<Record>, FoldError>;
}
/// Constrain even an in-memory view when retrying an older pending record.
struct HistoryPrefix<'a> {
    history: &'a dyn LibraryHistory,
    before: u64,
}
impl LibraryHistory for HistoryPrefix<'_> {
    fn lookup_fence(&self, sequence: u64) -> Result<Option<Record>, FoldError> {
        if sequence >= self.before {
            Ok(None)
        } else {
            self.history.lookup_fence(sequence)
        }
    }
    fn lookup(&self, r: DecisionRef) -> Result<Option<Record>, FoldError> {
        if r.sequence >= self.before {
            Ok(None)
        } else {
            self.history.lookup(r)
        }
    }
}
/// Maximum records held while native evidence is unavailable.
pub const MAX_PENDING_RECORDS: usize = 4096;
/// Maximum canonical CBOR bytes retained for pending envelopes and bodies.
pub const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

/// Resource isolation keys; wall clocks are never keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Tape UUID.
    Tape(Id),
    /// Live label.
    Label(LabelText),
    /// Drive UUID.
    Drive(Id),
    /// Physical endpoint.
    Endpoint(DriveKey),
    /// Changer identity.
    Library(Text),
    /// Operation identity.
    Operation(Id),
    /// Alarm condition.
    Alarm(Text),
    /// Accepted configuration.
    Config,
    /// Cleaning run.
    CleaningRun(Text),
}
/// Typed history contradictions and live-append refusals.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FoldError {
    /// The codec reserves this vocabulary but no fold behavior is defined yet.
    #[error("ReservedNotYetUsed")]
    ReservedNotYetUsed,
    /// A different medium requires a new incarnation.
    #[error("MediumFamilyChanged")]
    MediumFamilyChanged,
    /// Geometry and capabilities cannot change within an incarnation.
    #[error("ImmutableMediumField {{ field: {field} }}")]
    ImmutableMediumField {
        /// The fixed MediumSpec field that differed.
        field: &'static str,
    },
    /// Raising capacity requires fresh measured provenance on this revision.
    #[error("CapacityIncreaseRequiresMeasurement")]
    CapacityIncreaseRequiresMeasurement,
    /// A rule assignment must be recomputed under the current configuration.
    #[error("StaleConfiguration")]
    StaleConfiguration,
    /// Historical storage could not be read; never treat I/O failure as absence.
    #[error("HistoryUnavailable")]
    HistoryUnavailable,
    /// Native evidence could not be read; retry only after restoring access.
    #[error("native tape evidence unavailable: {0}")]
    TapeUnavailable(String),
    /// Refuse without dropping pending authority when either global cap is exceeded.
    #[error("PendingLimitExceeded {{ scope: {scope:?}, records: {records}, bytes: {bytes} }}")]
    PendingLimitExceeded {
        /// Resources affected by the refused record.
        scope: BTreeSet<Scope>,
        /// Prospective total retained pending records.
        records: usize,
        /// Prospective total canonical pending bytes.
        bytes: usize,
    },
    /// Refuses the `Sequence` history violation.
    #[error("Sequence")]
    Sequence,
    /// Refuses the `InvalidSchema` history violation.
    #[error("InvalidSchema")]
    InvalidSchema,
    /// Refuses the `UnknownTape` history violation.
    #[error("UnknownTape")]
    UnknownTape,
    /// A data-tape decision cannot name an inventory-only cleaning cartridge.
    #[error("WrongMediumKind")]
    WrongMediumKind,
    /// Refuses the `IncarnationMismatch` history violation.
    #[error("IncarnationMismatch")]
    IncarnationMismatch,
    /// Refuses the `RetiredIdentity` history violation.
    #[error("RetiredIdentity")]
    RetiredIdentity,
    /// Refuses the `GenerationMismatch` history violation.
    #[error("GenerationMismatch")]
    GenerationMismatch,
    /// Refuses the `ConflictingDuplicate` history violation.
    #[error("ConflictingDuplicate")]
    ConflictingDuplicate,
    /// The keyed operation already closed; B4b answers retries from its projection.
    #[error("ClosedOperationReplay {{ operation: {operation:?} }}")]
    ClosedOperationReplay {
        /// Operation whose terminal receipt is owned by the projection.
        operation: Id,
    },
    /// Refuses the `LabelHeld` history violation.
    #[error("LabelHeld")]
    LabelHeld,
    /// Refuses the `PriorHolder` history violation.
    #[error("PriorHolder")]
    PriorHolder,
    /// Refuses the `ObservedIdentity` history violation.
    #[error("ObservedIdentity")]
    ObservedIdentity,
    /// Refuses the `ForwardReference` history violation.
    #[error("ForwardReference")]
    ForwardReference,
    /// Refuses the `WrongCounterpart` history violation.
    #[error("WrongCounterpart")]
    WrongCounterpart,
    /// Refuses the `CounterpartKind` history violation.
    #[error("CounterpartKind")]
    CounterpartKind,
    /// Refuses the `Unresolved` history violation.
    #[error("Unresolved")]
    Unresolved,
    /// Refuses the `TapeContradiction` history violation.
    #[error("TapeContradiction")]
    TapeContradiction,
    /// A repair may not discard any earlier received dependent reference (D5).
    #[error("RepairBelowReference {{ highest: {highest} }}")]
    RepairBelowReference {
        /// Highest tape-log sequence referenced by an earlier received record.
        highest: u64,
    },
    /// Refuses the `FenceScopeMismatch` history violation.
    #[error("FenceScopeMismatch")]
    FenceScopeMismatch,
    /// Refuses the `ReleaseWithoutFence` history violation.
    #[error("ReleaseWithoutFence")]
    ReleaseWithoutFence,
    /// Refuses the `FenceAlreadyReleased` history violation.
    #[error("FenceAlreadyReleased")]
    FenceAlreadyReleased,
    /// Refuses the `OutcomeWithoutOpenIntent` history violation.
    #[error("OutcomeWithoutOpenIntent")]
    OutcomeWithoutOpenIntent,
    /// Refuses the `LoadAttempt` history violation.
    #[error("LoadAttempt")]
    LoadAttempt,
    /// Refuses the `Confirmation` history violation.
    #[error("Confirmation")]
    Confirmation,
    /// Refuses the `InventoryGeneration` history violation.
    #[error("InventoryGeneration")]
    InventoryGeneration,
    /// Refuses the `InventoryScope` history violation.
    #[error("InventoryScope")]
    InventoryScope,
    /// Refuses the `CoverageChanged` history violation.
    #[error("CoverageChanged")]
    CoverageChanged,
    /// Refuses the `ProgressRegressed` history violation.
    #[error("ProgressRegressed")]
    ProgressRegressed,
    /// Refuses the `ImpossibleCursor` history violation.
    #[error("ImpossibleCursor")]
    ImpossibleCursor,
    /// Refuses the `VerdictWithoutCoverage` history violation.
    #[error("VerdictWithoutCoverage")]
    VerdictWithoutCoverage,
    /// Refuses the `ConfigDigest` history violation.
    #[error("ConfigDigest")]
    ConfigDigest,
    /// Refuses the `Chain` history violation.
    #[error("Chain")]
    Chain,
    /// Refuses the `UnknownDrive` history violation.
    #[error("UnknownDrive")]
    UnknownDrive,
    /// Refuses the `DriveState` history violation.
    #[error("DriveState")]
    DriveState,
    /// Refuses the `CleaningBusy` history violation.
    #[error("CleaningBusy")]
    CleaningBusy,
    /// Refuses the `AlarmState` history violation.
    #[error("AlarmState")]
    AlarmState,
    /// Refuses the `SystemAcknowledgement` history violation.
    #[error("SystemAcknowledgement")]
    SystemAcknowledgement,
    /// Refuses the `OperationState` history violation.
    #[error("OperationState")]
    OperationState,
    /// Refuses the `SessionState` history violation.
    #[error("SessionState")]
    SessionState,
    /// Refuses the `CacheDigest` history violation.
    #[error("CacheDigest")]
    CacheDigest,
    /// Refuses the `OperationIdentity` history violation.
    #[error("OperationIdentity")]
    OperationIdentity,
}
/// Current identity and generation state of a provisioned tape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeState {
    /// Incarnation identity.
    pub tape: TapeRef,
    /// Retirement is permanent.
    pub retired: bool,
    /// Pool generation.
    pub assignment_generation: u64,
    /// Label generation.
    pub label_generation: u64,
    /// Media revision.
    pub media_revision: u64,
    /// Current capabilities, retained to validate subsequent revisions.
    pub medium: MediumSpec,
    /// Whether any committed spec in this incarnation had measured provenance.
    pub ever_measured: bool,
    /// Live shell label.
    pub label: Option<LabelText>,
    /// Current pool.
    pub pool: Option<Text>,
    /// Who decided the current pool, from the latest `Reprovision` or `AssignPool`; `None` until a
    /// pool decision is recorded.
    pub pool_source: Option<PoolSource>,
    /// Current calibration generation.
    pub calibration_generation: u64,
    /// Opaque map digest only; projectors read descriptors from the record.
    pub wrap_digest: Option<Hash>,
}
/// Inventory-only identity: no BOT, data geometry or incarnation history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleaningCartridgeState {
    /// Barcode is the sole physical identity.
    pub barcode: LabelText,
    /// Last registered home slot.
    pub home_slot: u16,
    /// Expiry is permanent; the identity remains reserved.
    pub expired: bool,
    /// Failed corroboration permanently prevents selection, but permits run closure.
    pub rejected: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Held {
    sequence: u64,
    record: Record,
    receipt: Receipt,
}
/// An unresolved record with stable causal identity and affected scopes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingRecord {
    /// Library sequence.
    pub sequence: u64,
    /// Kind.
    pub tag: Tag,
    /// Referenced proof, when this is the direct dependency.
    pub key: Option<TapeAt>,
    /// All scopes queued behind this record.
    pub scope: BTreeSet<Scope>,
    bytes: Box<[u8]>,
    receipt: Receipt,
    deferred_fence: bool,
}
/// Result of validation, reused by append and replay.
#[derive(Clone, Debug)]
pub enum Validated {
    /// The candidate can change state.
    Apply,
    /// Existing identical authority; no bytes should be appended.
    Duplicate(Receipt),
}
/// Bounded tape-reference authority and replay diagnostics (D3/D9).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TapeReferences {
    /// Highest received native reference, its digest and the first naming library sequence.
    pub floor: Option<(u64, Hash, u64)>,
    /// Every digest received at the current floor, including conflicting evidence.
    pub floor_digests: BTreeSet<Hash>,
    /// Lowest mismatching native sequence, if any.
    pub minimum_contradiction: Option<u64>,
    /// Highest dependent reference received, independent of its disposition.
    pub highest_reference: Option<u64>,
    /// Number of excluded records; diagnostic only, never an admission rule.
    pub excluded: u64,
}
impl TapeReferences {
    /// Contradiction status is derived from the minimum, never map membership.
    pub fn contradicted(&self) -> bool {
        self.minimum_contradiction.is_some()
    }
}

/// O(resources), plus 80 bytes of key/value data per keyed operation (map overhead
/// measured separately), lifetime crash supersedes/unprotected acceptances,
/// pending up to the cap, and one looked-up frame.
/// Pending bodies are canonical bytes, not a retained decoded record history.
#[derive(Clone, Debug, Default)]
pub struct LibraryFold {
    /// Last received frame.
    pub received_through: u64,
    /// Highest contiguous decided prefix (including exclusions), never crossing pending.
    pub applied_through: u64,
    /// Number of identical duplicate records encountered on replay.
    pub duplicates: u64,
    /// Received positions decided without an application (observer bookkeeping only).
    pub(crate) non_applications: u64,
    /// Current tape identities.
    pub tapes: BTreeMap<Id, TapeState>,
    /// Cleaning identities are separate from data tapes and retain no MediumSpec.
    pub cleaning_cartridges: BTreeMap<Id, CleaningCartridgeState>,
    /// Pending records in source sequence order.
    pub pending: Vec<PendingRecord>,
    /// Per-tape summaries, including tapes with matching or pending references.
    pub contradictions: BTreeMap<Scope, TapeReferences>,
    /// Failed deferred liveness checks, attached to their open fence and full identity.
    pub fence_liveness_failures: BTreeMap<u64, (TapeRef, FoldError)>,
    /// Missing fence evidence, which never delays the fence.
    pub unresolved_fence_evidence: BTreeSet<u64>,
    /// Exact observation remembered by the latest forced cause-14 release.
    pub forced_observation: BTreeMap<Id, Bounded<DiagnosticEntry, 256>>,
    pending_bytes: usize,
    /// Most recent pending retry refusal, observed without changing pending retention.
    pub pending_rejection: Option<(u64, Id)>,
    latest: Index<(Tag, Scope, Option<Id>), Held>,
    fences: Index<u64, Held>,
    tape_fences: Index<Id, BTreeSet<u64>>,
    supersedes: Index<(TapeRef, Option<Id>), Held>,
    supersede_sequences: Index<u64, (TapeRef, Option<Id>)>,
    unprotected: Index<(TapeRef, Id), Held>,
    intents: Index<u64, Held>,
    uncertain: BTreeSet<u64>,
    loads: Index<(Id, DriveKey), u32>,
    inventories: Index<InventorySubject, u64>,
    drives: Index<Id, (u64, bool)>,
    readiness: Index<DriveKey, u64>,
    // Retained across Refresh so a second acknowledgement stays forbidden.
    alarm_acknowledgers: BTreeMap<Text, AuditActor>,
    idempotency: Index<Hash, (Id, Hash)>,
    by_sequence: Index<u64, (Tag, Scope, Option<Id>)>,
    operation_slots: Index<Id, BTreeSet<(Tag, Scope, Option<Id>)>>,
    load_endpoints: Index<Id, BTreeSet<DriveKey>>,
    released: Index<u64, (Tag, Scope, Option<Id>)>,
    labels: Index<LabelText, Id>,
    commands: Index<Id, u64>,
    intent_scopes: Index<Scope, usize>,
    cleaning_drives: Index<Id, Text>,
    cleaning_tapes: Index<Id, Text>,
    // Active runs retain their selected cartridge when later records omit it.
    cleaning_run_cartridges: Index<Text, TapeRef>,
    cleaning_run_homes: Index<Text, u16>,
    lookup_count: ProbeCounter,
}
impl Record {
    /// Reference that a later decision may bind.
    pub fn reference(&self, sequence: u64) -> DecisionRef {
        DecisionRef {
            sequence,
            record_uuid: self.envelope.record_uuid,
        }
    }
}
impl Body {
    pub(super) fn operation(&self) -> Option<Id> {
        match self {
            Self::SupersedeTail(v) => v.operation,
            Self::AcceptUnprotectedFinalization(v) => Some(v.operation),
            Self::VerificationProgress(v) => Some(v.operation),
            Self::VerificationVerdict(v) => Some(v.operation),
            Self::ManualFinalizationAccepted(v) => Some(v.operation),
            Self::ReadinessObserved(v) => Some(v.operation),
            Self::OperationRecorded(v) => Some(v.operation),
            Self::RecoveryRecorded(v) => Some(v.operation),
            Self::FenceRaised(v) => v.operation,
            Self::MotionIntent(v) => v.parent_operation,
            Self::DriveHealthObserved(v) => v.operation,
            Self::Reprovision(v) => v.adoption.as_ref().map(|a| a.operation),
            _ => None,
        }
    }
    pub(super) fn at(&self) -> Option<&TapeAt> {
        match self {
            Self::SupersedeTail(v) => Some(&v.at),
            Self::AcceptUnprotectedFinalization(v) => Some(&v.at),
            Self::VerificationProgress(v) => Some(&v.at),
            Self::VerificationVerdict(v) => v.at.as_ref(),
            Self::ManualFinalizationAccepted(v) => Some(&v.at),
            Self::RecoveryRecorded(v) => Some(&v.at),
            _ => None,
        }
    }
    fn tapes(&self) -> Vec<TapeRef> {
        match self {
            Self::AssignPool(v) => vec![v.tape],
            Self::VerificationVerdict(v) => vec![v.tape],
            Self::Label(v) => std::iter::once(v.tape)
                .chain(v.prior_holder.as_ref().map(|x| x.tape))
                .collect(),
            Self::Retire(v) => vec![v.tape],
            Self::MediumSpecSet(v) => vec![v.tape],
            Self::WrapMapCached(v) => vec![v.tape],
            // Evidence never participates in fence liveness or queue scopes.
            Self::FenceRaised(v) => match &v.scope {
                FenceScope::Tape(t) => vec![*t],
                _ => vec![],
            },
            Self::MotionIntent(v) => v.tape.into_iter().collect(),
            Self::ReadinessObserved(v) => v.tape.into_iter().collect(),
            Self::DriveHealthObserved(v) => v.tape.into_iter().collect(),
            Self::CleaningRecorded(v) => v.cartridge.into_iter().collect(),
            Self::SessionObserved(v) => v.tape.into_iter().collect(),
            _ => self.at().map(|a| vec![a.tape]).unwrap_or_default(),
        }
    }
    fn refs(&self) -> Vec<DecisionRef> {
        match self {
            Self::AssignPool(v) => v.configuration.into_iter().collect(),
            Self::Reprovision(v) => match v.pool_source {
                Some(PoolSource::Rule(reference)) => vec![reference],
                _ => vec![],
            },
            Self::MotionOutcome(v) => vec![v.intent],
            Self::InventoryReconciled(v) => v.resolves.0.clone(),
            Self::VerificationVerdict(v) => v.progress.into_iter().collect(),
            Self::ReadinessObserved(v) => v.previous.into_iter().chain(v.load_intent).collect(),
            Self::CleaningRecorded(v) => v.previous.into_iter().collect(),
            Self::AlarmChanged(v) => v
                .cause
                .into_iter()
                .chain(match &v.action {
                    AlarmAction::Refresh(a) => a.cause,
                    _ => None,
                })
                .collect(),
            Self::OperationRecorded(v) => v.previous.into_iter().collect(),
            Self::SessionObserved(v) => v.previous.into_iter().collect(),
            Self::RecoveryRecorded(v) => v.prior.into_iter().collect(),
            _ => vec![],
        }
    }
    /// All resources whose relative order this record constrains.
    pub fn scopes(&self) -> BTreeSet<Scope> {
        let mut s: BTreeSet<_> = self.tapes().iter().map(|t| Scope::Tape(t.uuid)).collect();
        if let Some(op) = self.operation() {
            s.insert(Scope::Operation(op));
        }
        match self {
            Self::Reprovision(v) => {
                s.insert(Scope::Tape(v.tape.uuid));
                if let Some(t) = v.predecessor {
                    s.insert(Scope::Tape(t.uuid));
                }
                if let Some(l) = &v.label {
                    s.insert(Scope::Label(l.clone()));
                }
            }
            Self::Label(v) => {
                if let Some(l) = &v.label {
                    s.insert(Scope::Label(l.clone()));
                }
            }
            Self::FenceRaised(v) => {
                if let FenceScope::Drive(d) = &v.scope {
                    s.insert(Scope::Endpoint(d.clone()));
                }
            }
            Self::MotionIntent(v) => {
                s.extend(motion_scopes(&v.motion));
                if let Some(op) = v.parent_operation {
                    s.insert(Scope::Operation(op));
                }
            }
            Self::InventoryReconciled(v) => {
                s.insert(inventory_scope(&v.subject));
            }
            Self::ConfigurationAccepted(_) => {
                s.insert(Scope::Config);
            }
            Self::ReadinessObserved(v) => {
                s.insert(Scope::Endpoint(v.endpoint.clone()));
                if v.scope == ReadinessScope::Library {
                    if let DriveKey::Bay(bay) = &v.endpoint {
                        s.insert(Scope::Library(bay.library_serial.clone()));
                    }
                }
            }
            Self::DriveObserved(v) => {
                s.insert(Scope::Drive(v.drive_uuid));
                if let Some(d) = &v.endpoint {
                    s.insert(Scope::Endpoint(d.clone()));
                }
            }
            Self::DriveDecision(v) => {
                s.insert(Scope::Drive(v.drive_uuid));
            }
            Self::DriveHealthObserved(v) => {
                s.insert(Scope::Drive(v.drive_uuid));
            }
            Self::CleaningRecorded(v) => {
                s.insert(Scope::CleaningRun(v.run_id.clone()));
                if let Some(d) = v.drive_uuid {
                    s.insert(Scope::Drive(d));
                }
            }
            Self::AlarmChanged(v) => {
                s.insert(Scope::Alarm(v.condition_key.clone()));
            }
            Self::SessionObserved(v) => {
                s.insert(Scope::Operation(v.session_id));
                if let Some(d) = &v.endpoint {
                    s.insert(Scope::Endpoint(d.clone()));
                }
            }
            _ => {}
        }
        s
    }
    fn slot_tape(&self) -> Option<Id> {
        match self {
            Self::SupersedeTail(v) => Some(v.at.tape.uuid),
            Self::AcceptUnprotectedFinalization(v) => Some(v.at.tape.uuid),
            _ => None,
        }
    }
    fn slot(&self) -> Option<Scope> {
        match self {
            Self::Reprovision(v) => Some(Scope::Tape(v.tape.uuid)),
            Self::AssignPool(v) => Some(Scope::Tape(v.tape.uuid)),
            Self::Label(v) => Some(Scope::Tape(v.tape.uuid)),
            Self::Retire(v) => Some(Scope::Tape(v.tape.uuid)),
            Self::MediumSpecSet(v) => Some(Scope::Tape(v.tape.uuid)),
            Self::ManualFinalizationAccepted(v) => Some(Scope::Tape(v.at.tape.uuid)),
            Self::WrapMapCached(_) | Self::MotionIntent(_) | Self::FenceRaised(_) => None,
            Self::DriveObserved(v) => Some(Scope::Drive(v.drive_uuid)),
            Self::DriveDecision(v) => Some(Scope::Drive(v.drive_uuid)),
            Self::DriveHealthObserved(v) => Some(Scope::Drive(v.drive_uuid)),
            Self::ConfigurationAccepted(_) => Some(Scope::Config),
            Self::AlarmChanged(v) => Some(Scope::Alarm(v.condition_key.clone())),
            Self::CleaningRecorded(v) => {
                if matches!(
                    v.action,
                    CleaningAction::Register(_)
                        | CleaningAction::Expire(_)
                        | CleaningAction::Reject(_)
                ) {
                    v.cartridge.map(|tape| Scope::Tape(tape.uuid))
                } else {
                    Some(Scope::CleaningRun(v.run_id.clone()))
                }
            }
            Self::SessionObserved(v) => Some(Scope::Operation(v.session_id)),
            Self::SupersedeTail(_) | Self::AcceptUnprotectedFinalization(_) => None,
            _ => self.operation().map(Scope::Operation),
        }
    }
}
fn motion_scopes(m: &Motion) -> BTreeSet<Scope> {
    match m {
        Motion::Load(v) => endpoint_scopes(&v.endpoint),
        Motion::Unload(v) => endpoint_scopes(&v.endpoint),
        Motion::Move(v) => BTreeSet::from([Scope::Library(v.library_serial.clone())]),
    }
}
fn endpoint_scopes(d: &DriveKey) -> BTreeSet<Scope> {
    let mut s = BTreeSet::from([Scope::Endpoint(d.clone())]);
    if let DriveKey::Bay(v) = d {
        s.insert(Scope::Library(v.library_serial.clone()));
    }
    s
}
fn generation(c: &GenerationChange, current: u64) -> Result<(), FoldError> {
    if c.expected != current || current.checked_add(1) != Some(c.next) {
        Err(FoldError::GenerationMismatch)
    } else {
        Ok(())
    }
}
fn terminal(e: &OperationEvent) -> bool {
    matches!(
        e,
        OperationEvent::Finished(_)
            | OperationEvent::Failed(_)
            | OperationEvent::CancelledBeforeDispatch(_)
            | OperationEvent::CompletedAfterCancel(_)
            | OperationEvent::CompletionUnknown(_)
    )
}
impl LibraryFold {
    fn tape(&self, t: TapeRef) -> Result<&TapeState, FoldError> {
        self.probe();
        if self.cleaning_cartridges.contains_key(&t.uuid) {
            return Err(FoldError::WrongMediumKind);
        }
        let v = self.tapes.get(&t.uuid).ok_or(FoldError::UnknownTape)?;
        if v.tape != t {
            return Err(FoldError::IncarnationMismatch);
        }
        if v.retired {
            return Err(FoldError::RetiredIdentity);
        }
        Ok(v)
    }
    /// Motion, cleaning and health may name inventory media without data geometry.
    fn inventory_medium(&self, t: TapeRef) -> Result<(), FoldError> {
        if let Some(c) = self.cleaning_cartridges.get(&t.uuid) {
            if t.incarnation != 1 {
                return Err(FoldError::IncarnationMismatch);
            }
            if c.expired {
                return Err(FoldError::RetiredIdentity);
            }
            Ok(())
        } else {
            self.tape(t).map(|_| ())
        }
    }
    /// Keyed reads/updates in retained-state indexes, plus public tape-state probes.
    /// Hash collision and allocator costs are excluded; indexes expose no iteration.
    pub fn lookup_count(&self) -> u64 {
        self.lookup_count.get()
            + self.latest.probes()
            + self.fences.probes()
            + self.tape_fences.probes()
            + self.supersedes.probes()
            + self.supersede_sequences.probes()
            + self.unprotected.probes()
            + self.intents.probes()
            + self.loads.probes()
            + self.inventories.probes()
            + self.drives.probes()
            + self.readiness.probes()
            + self.idempotency.probes()
            + self.by_sequence.probes()
            + self.operation_slots.probes()
            + self.load_endpoints.probes()
            + self.released.probes()
            + self.labels.probes()
            + self.commands.probes()
            + self.intent_scopes.probes()
            + self.cleaning_drives.probes()
            + self.cleaning_tapes.probes()
            + self.cleaning_run_cartridges.probes()
    }
    /// Fixed key/value payload retained for lifetime idempotency.
    pub fn idempotency_bytes(&self) -> usize {
        self.idempotency.len() * 80
    }
    /// Retained decoded records, excluding bounded pending bytes.
    pub fn retained_records(&self) -> usize {
        self.latest.len()
            + self.fences.len()
            + self.intents.len()
            + self.supersedes.len()
            + self.unprotected.len()
    }
    fn probe(&self) {
        self.lookup_count.increment();
    }
    fn at_sequence(&self, sequence: u64) -> Option<&Held> {
        if let Some(key) = self.by_sequence.get(&sequence) {
            return self.latest.get(key);
        }
        self.fences
            .get(&sequence)
            .or_else(|| self.intents.get(&sequence))
    }
    fn find(&self, r: DecisionRef) -> Option<&Held> {
        self.at_sequence(r.sequence)
            .filter(|h| h.record.envelope.record_uuid == r.record_uuid)
    }
    fn remove_latest(&mut self, key: &(Tag, Scope, Option<Id>)) {
        if let Some(old) = self.latest.remove(key) {
            self.by_sequence.remove(&old.sequence);
            if let Body::FenceReleased(v) = &old.record.body {
                self.released.remove(&v.fence);
            }
            if let Scope::Operation(scope_id) = &key.1 {
                let op = old.record.body.operation().unwrap_or(*scope_id);
                if let Some(keys) = self.operation_slots.get_mut(&op) {
                    keys.remove(key);
                    if keys.is_empty() {
                        self.operation_slots.remove(&op);
                    }
                }
            }
        }
    }
    fn insert_latest(&mut self, key: (Tag, Scope, Option<Id>), held: Held) {
        self.remove_latest(&key);
        self.by_sequence.insert(held.sequence, key.clone());
        if let Scope::Operation(scope_id) = &key.1 {
            let op = held.record.body.operation().unwrap_or(*scope_id);
            self.operation_slots
                .entry(op)
                .or_default()
                .insert(key.clone());
        }
        if let Body::FenceReleased(v) = &held.record.body {
            self.released.insert(v.fence, key.clone());
        }
        self.latest.insert(key, held);
    }
    fn close_operation(&mut self, operation: Id) {
        if let Some(keys) = self.operation_slots.remove(&operation) {
            for key in keys {
                self.remove_latest(&key);
            }
        }
        self.close_loads(operation);
    }
    fn close_loads(&mut self, operation: Id) {
        if let Some(endpoints) = self.load_endpoints.remove(&operation) {
            for endpoint in endpoints {
                self.loads.remove(&(operation, endpoint));
            }
        }
    }
    fn remove_intent(&mut self, sequence: u64) {
        if let Some(h) = self.intents.remove(&sequence) {
            if let Body::MotionIntent(v) = &h.record.body {
                self.commands.remove(&v.command);
            }
            for scope in h.record.body.scopes() {
                let count = self.intent_scopes.get_mut(&scope).unwrap();
                *count -= 1;
                if *count == 0 {
                    self.intent_scopes.remove(&scope);
                }
            }
        }
        self.uncertain.remove(&sequence);
    }
    fn historical<'a>(
        &'a self,
        r: DecisionRef,
        history: &dyn LibraryHistory,
    ) -> Result<Option<Cow<'a, Record>>, FoldError> {
        if let Some(h) = self.find(r) {
            return Ok(Some(Cow::Borrowed(&h.record)));
        }
        Ok(history
            .lookup(r)?
            .filter(|record| record.envelope.record_uuid == r.record_uuid)
            .map(Cow::Owned))
    }
    fn typed(
        &self,
        r: DecisionRef,
        tag: Tag,
        missing: FoldError,
        history: &dyn LibraryHistory,
    ) -> Result<&Held, FoldError> {
        let Some(h) = self.find(r) else {
            // History classifies wrong kinds without reopening resolved intents.
            if self
                .historical(r, history)?
                .is_some_and(|record| record.body.tag() != tag)
            {
                return Err(FoldError::WrongCounterpart);
            }
            return Err(missing);
        };
        if h.record.body.tag() != tag {
            return Err(FoldError::WrongCounterpart);
        }
        Ok(h)
    }
    fn last(&self, tag: Tag, scope: Scope) -> Option<&Held> {
        self.latest.get(&(tag, scope, None))
    }
    fn chain(
        &self,
        tag: Tag,
        scope: Scope,
        previous: Option<DecisionRef>,
    ) -> Result<Option<&Held>, FoldError> {
        let last = self.last(tag, scope);
        if last.map(|h| h.record.reference(h.sequence)) != previous {
            return Err(FoldError::Chain);
        }
        Ok(last)
    }
    /// Highest received dependent reference; repair writers check this before repair_tail.
    pub fn highest_reference(&self, tape: TapeRef) -> Option<u64> {
        self.contradictions
            .get(&Scope::Tape(tape.uuid))?
            .highest_reference
    }
    /// Whether replay has excluded any reference on this tape.
    pub fn contradicted(&self, tape: TapeRef) -> bool {
        self.contradictions
            .get(&Scope::Tape(tape.uuid))
            .is_some_and(TapeReferences::contradicted)
    }
    /// Number of contradicted tapes; summaries also retain non-contradicted tapes.
    pub fn contradicted_tapes(&self) -> usize {
        self.contradictions
            .values()
            .filter(|s| s.contradicted())
            .count()
    }
    fn note_reference(&mut self, at: Option<&TapeAt>) {
        if let Some(at) = at {
            let summary = self
                .contradictions
                .entry(Scope::Tape(at.tape.uuid))
                .or_default();
            summary.highest_reference =
                Some(summary.highest_reference.unwrap_or(0).max(at.sequence));
        }
    }
    /// Preserve received evidence independently of application and repair policy.
    fn note_floor(&mut self, at: Option<&TapeAt>, sequence: u64) {
        let Some(at) = at else { return };
        let summary = self
            .contradictions
            .entry(Scope::Tape(at.tape.uuid))
            .or_default();
        match summary.floor {
            Some((floor, _, _)) if floor > at.sequence => {}
            Some((floor, _, _)) if floor == at.sequence => {
                summary.floor_digests.insert(at.record_digest);
            }
            _ => {
                summary.floor = Some((at.sequence, at.record_digest, sequence));
                summary.floor_digests = BTreeSet::from([at.record_digest]);
            }
        }
    }
    fn exclude(&mut self, at: &TapeAt) {
        let summary = self
            .contradictions
            .entry(Scope::Tape(at.tape.uuid))
            .or_default();
        summary.minimum_contradiction = Some(
            summary
                .minimum_contradiction
                .unwrap_or(at.sequence)
                .min(at.sequence),
        );
        summary.excluded += 1;
    }
    // Check once on admission/receipt, not again when resolving an older pending repair:
    // later references must not retroactively invalidate that repair.
    fn check_repair_floor(&self, b: &Body) -> Result<(), FoldError> {
        if let Body::RecoveryRecorded(v) = b {
            if let RecoveryAction::RepairApplied(repair) = &v.action {
                if let Some(highest) = self.highest_reference(v.at.tape) {
                    if highest > repair.retained_through {
                        return Err(FoldError::RepairBelowReference { highest });
                    }
                }
            }
        }
        Ok(())
    }
    fn check_dependent_identity(&self, b: &Body) -> Result<(), FoldError> {
        // Medium kind is decided before queueing and before native proof lookup.
        // A cleaning cartridge can never acquire data geometry through a queued record.
        if !matches!(
            b,
            Body::CleaningRecorded(_) | Body::MotionIntent(_) | Body::DriveHealthObserved(_)
        ) {
            for t in b.tapes() {
                if self.cleaning_cartridges.contains_key(&t.uuid) {
                    return Err(FoldError::WrongMediumKind);
                }
            }
        }
        if let Body::Reprovision(v) = b {
            if self.cleaning_cartridges.contains_key(&v.tape.uuid)
                || v.predecessor
                    .is_some_and(|p| self.cleaning_cartridges.contains_key(&p.uuid))
            {
                return Err(FoldError::WrongMediumKind);
            }
        }
        if b.at().is_some() {
            for tape in b.tapes() {
                self.tape(tape)?;
            }
        }
        Ok(())
    }
    // Scope-held dependencies still validate identity before their own native reference.
    fn check_reference(&self, b: &Body, view: &dyn TapeView) -> Result<(), FoldError> {
        if b.at().is_some() {
            self.check_dependent_identity(b)?;
            self.check_tape(b, view)?;
        }
        Ok(())
    }
    fn check_tape(&self, b: &Body, view: &dyn TapeView) -> Result<(), FoldError> {
        if let Some(at) = b.at() {
            match view.lookup(at)? {
                TapeLookup::NotYet => return Err(FoldError::Unresolved),
                TapeLookup::Contradiction => return Err(FoldError::TapeContradiction),
                TapeLookup::EmptyPrefix { digest } => {
                    if at.sequence != 0 || digest != at.record_digest {
                        return Err(FoldError::TapeContradiction);
                    }
                }
                TapeLookup::Present { kind, digest } => {
                    if at.sequence == 0 {
                        return Err(FoldError::CounterpartKind);
                    }
                    if digest != at.record_digest {
                        return Err(FoldError::TapeContradiction);
                    }
                    if matches!(b, Body::ManualFinalizationAccepted(_))
                        && kind != crate::tape_log::Tag::FinalizationPrepared
                    {
                        return Err(FoldError::CounterpartKind);
                    }
                }
            }
        }
        Ok(())
    }
    /// Pure validation for a prospective append at the next source sequence.
    pub fn validate(
        &self,
        r: &Record,
        view: &dyn TapeView,
        history: &dyn LibraryHistory,
    ) -> Result<Validated, FoldError> {
        self.validate_shape(self.received_through + 1, r)?;
        if let Body::FenceRaised(FenceRaised { at: Some(at), .. }) = &r.body {
            // Evidential references may be missing or mismatched without
            // blocking a safety fence. An unreadable view is different: catch
            // that before publication, including the deferred-fence fast path.
            view.lookup(at)?;
        }
        let history = &HistoryPrefix {
            history,
            before: self.received_through + 1,
        };
        self.check_dependent_identity(&r.body)?;
        self.check_repair_floor(&r.body)?;
        let scopes = self.scopes_for(&r.body, history)?;
        if matches!(r.body, Body::FenceRaised(_)) && self.blocked(&scopes) {
            self.validate_deferred_fence(r, &scopes)?;
            return Ok(Validated::Apply);
        }
        if !matches!(r.body, Body::FenceRaised(_))
            && self.pending.iter().any(|p| !p.scope.is_disjoint(&scopes))
        {
            self.check_pending_capacity(r, &scopes)?;
            return Err(FoldError::Unresolved);
        }
        self.validate_at(self.received_through + 1, r, view, history)
    }
    fn validate_shape(&self, sequence: u64, r: &Record) -> Result<(), FoldError> {
        let b = &r.body;
        let modes = match b {
            Body::VerificationProgress(v) => Some(v.modes),
            Body::VerificationVerdict(v) => {
                if v.at.as_ref().is_some_and(|at| at.tape != v.tape)
                    || (v.outcome == VerificationOutcome::VerifiedComplete
                        && !v.damaged.0.is_empty())
                {
                    return Err(FoldError::InvalidSchema);
                }
                Some(v.modes)
            }
            _ => None,
        };
        if modes.is_some_and(|modes| modes == 0 || modes & !0x7f != 0) {
            return Err(FoldError::InvalidSchema);
        }
        // Diagnostic and fence evidence share one canonical detail representation.
        let detail = match b {
            Body::FenceRaised(v) => Some(&v.detail),
            Body::FenceReleased(v) => Some(&v.detail),
            Body::Diagnostic(v) => Some(&v.detail),
            _ => None,
        };
        if detail.is_some_and(|d| d.0.windows(2).any(|w| w[0].0 >= w[1].0)) {
            return Err(FoldError::InvalidSchema);
        }
        if let Body::InventoryReconciled(v) = b {
            if matches!(v.subject, InventorySubject::Standalone(_))
                && (!v.complete || !v.elements.0.is_empty())
            {
                return Err(FoldError::InvalidSchema);
            }
        }
        if matches!(b, Body::Reprovision(v) if v.replaced_bot.is_some()
            && (v.mode != ProvisionMode::Initialize || v.predecessor.is_some()))
        {
            return Err(FoldError::InvalidSchema);
        }
        // A pool names the decision that set it, and only a pool does. `operation` has no
        // presence rule yet.
        if matches!(b, Body::Reprovision(v) if v.pool.is_some() != v.pool_source.is_some()) {
            return Err(FoldError::InvalidSchema);
        }
        if let Body::ReadinessObserved(v) = b {
            if v.tape.is_some() && v.media.is_some() {
                return Err(FoldError::InvalidSchema);
            }
            if v.scope == ReadinessScope::Library && !matches!(v.endpoint, DriveKey::Bay(_)) {
                return Err(FoldError::InvalidSchema);
            }
        }
        if let Body::CleaningRecorded(v) = b {
            if let CleaningAction::Advance(a) = &v.action {
                if (a.phase == CleanPhase::MovingIn) != a.home_slot.is_some()
                    || (a.phase == CleanPhase::MovingIn && v.cartridge.is_none())
                {
                    return Err(FoldError::InvalidSchema);
                }
            }
        }
        if matches!(b, Body::FenceRaised(v) if v.cause == FenceCause::ContainerQuarantine
            || matches!(v.scope, FenceScope::Model { .. }))
        {
            return Err(FoldError::ReservedNotYetUsed);
        }
        // Codec bounds and canonical scalars are shared by live and replay validation.
        let mut sink = crate::log_frame::Limited {
            bytes: Vec::new(),
            limit: crate::log_frame::MAX_FRAME_BODY - 32,
        };
        wire::encode(r, &mut sink).map_err(|_| FoldError::InvalidSchema)?;
        if b.operation()
            .zip(r.envelope.operation_id)
            .is_some_and(|(a, b)| a != b)
        {
            return Err(FoldError::OperationIdentity);
        }
        if b.refs()
            .iter()
            .any(|d| d.sequence == 0 || d.sequence >= sequence)
            || matches!(b,Body::FenceReleased(v) if v.fence==0||v.fence>=sequence)
        {
            return Err(FoldError::ForwardReference);
        }
        if b.tapes().iter().any(|t| t.incarnation == 0) {
            return Err(FoldError::InvalidSchema);
        }
        let at = b.at().or(match b {
            Body::FenceRaised(v) => v.at.as_ref(),
            _ => None,
        });
        if at.is_some_and(|at| {
            (at.sequence == 0 && !matches!(b, Body::SupersedeTail(_))) || at.tape.incarnation == 0
        }) {
            return Err(FoldError::InvalidSchema);
        }
        if matches!(b, Body::RecoveryRecorded(v) if matches!(&v.action,
            RecoveryAction::RepairApplied(p) if p.retained_through != v.at.sequence))
        {
            return Err(FoldError::InvalidSchema);
        }
        Ok(())
    }
    fn validate_at(
        &self,
        sequence: u64,
        r: &Record,
        view: &dyn TapeView,
        history: &dyn LibraryHistory,
    ) -> Result<Validated, FoldError> {
        self.validate_shape(sequence, r)?;
        let history = &HistoryPrefix {
            history,
            before: sequence,
        };
        let b = &r.body;
        // A retry computed under a superseded configuration is stale too.
        // This check intentionally precedes retained-body duplicate detection.
        let rule_configuration = match b {
            Body::AssignPool(v) if v.source == AssignmentSource::Rule => {
                Some(v.configuration.ok_or(FoldError::WrongCounterpart)?)
            }
            Body::Reprovision(v) => match v.pool_source {
                Some(PoolSource::Rule(reference)) => Some(reference),
                _ => None,
            },
            _ => None,
        };
        if let Some(reference) = rule_configuration {
            let latest = self
                .last(Tag::ConfigurationAccepted, Scope::Config)
                .ok_or(FoldError::WrongCounterpart)?;
            if latest.record.reference(latest.sequence) != reference {
                return Err(
                    if self
                        .historical(reference, history)?
                        .is_some_and(|r| r.body.tag() == Tag::ConfigurationAccepted)
                    {
                        FoldError::StaleConfiguration
                    } else {
                        FoldError::WrongCounterpart
                    },
                );
            }
        }
        // Retirement dominates duplicate handling: an old decision cannot resurrect an identity.
        for t in b.tapes() {
            if t.incarnation == 0 {
                return Err(FoldError::InvalidSchema);
            }
            match b {
                Body::CleaningRecorded(v) if matches!(v.action, CleaningAction::Register(_)) => {}
                Body::CleaningRecorded(_)
                | Body::MotionIntent(_)
                | Body::DriveHealthObserved(_) => self.inventory_medium(t)?,
                _ => {
                    self.tape(t)?;
                }
            }
        }
        // Rejection is cartridge authority, not a run transition. Failed closure and
        // return motion remain possible after Reject; selection and credit do not.
        if let Body::CleaningRecorded(v) = b {
            if let Some(t) = v.cartridge {
                if matches!(v.action, CleaningAction::Reject(_))
                    && !self.cleaning_cartridges.contains_key(&t.uuid)
                {
                    return Err(FoldError::WrongMediumKind);
                }
                if self
                    .cleaning_cartridges
                    .get(&t.uuid)
                    .is_some_and(|c| c.rejected)
                    && matches!(
                        v.action,
                        CleaningAction::Reject(_)
                            | CleaningAction::Begin(_)
                            | CleaningAction::Advance(_)
                            | CleaningAction::Finish(Finish {
                                verified_credit: true,
                                ..
                            })
                    )
                {
                    return Err(FoldError::RetiredIdentity);
                }
            }
        }
        // Registration is identity creation, including on an identical retry.
        // Ruling 17 permits Retire then Register when a data UUID collides with
        // the barcode UUID. The retired data identity remains historical.
        if let Body::CleaningRecorded(v) = b {
            if let CleaningAction::Register(a) = &v.action {
                let t = v.cartridge.ok_or(FoldError::Chain)?;
                if self.tapes.get(&t.uuid).is_some_and(|data| !data.retired)
                    || self.cleaning_cartridges.contains_key(&t.uuid)
                {
                    return Err(FoldError::ConflictingDuplicate);
                }
                if a.barcode.is_empty()
                    || a.barcode.as_str().trim() != a.barcode.as_str()
                    || t != cleaning_cartridge_ref(&a.barcode)
                {
                    return Err(FoldError::InvalidSchema);
                }
            }
        }
        self.check_tape(b, view)?;
        if let Body::FenceReleased(v) = b {
            if let Some(old) = self
                .released
                .get(&v.fence)
                .and_then(|key| self.latest.get(key))
            {
                return if old.record.body == *b {
                    Ok(Validated::Duplicate(old.receipt.clone()))
                } else {
                    Err(FoldError::FenceAlreadyReleased)
                };
            }
        }
        let lifetime = match b {
            Body::SupersedeTail(v) => self.supersedes.get(&(v.at.tape, v.session_id)),
            Body::AcceptUnprotectedFinalization(v) => {
                self.unprotected.get(&(v.at.tape, v.operation))
            }
            _ => None,
        };
        if let Some(old) = lifetime {
            return if old.record.body == *b {
                Ok(Validated::Duplicate(old.receipt.clone()))
            } else {
                Err(FoldError::ConflictingDuplicate)
            };
        }
        if let Some(slot) = b.slot() {
            if let Some(old) = self.latest.get(&(b.tag(), slot, b.slot_tape())) {
                if duplicate_key(&old.record.body, b) {
                    return if old.record.body == *b {
                        Ok(Validated::Duplicate(old.receipt.clone()))
                    } else {
                        Err(FoldError::ConflictingDuplicate)
                    };
                }
            }
        }
        use Body::*;
        match b {
            Reprovision(v) => {
                if self.cleaning_cartridges.contains_key(&v.tape.uuid)
                    || v.predecessor
                        .is_some_and(|p| self.cleaning_cartridges.contains_key(&p.uuid))
                {
                    return Err(FoldError::WrongMediumKind);
                }
                if self.tapes.contains_key(&v.tape.uuid) || v.tape.incarnation != 1 {
                    return Err(FoldError::ConflictingDuplicate);
                }
                if let Some(p) = v.predecessor {
                    if !self
                        .tapes
                        .get(&p.uuid)
                        .is_some_and(|t| t.tape == p && t.retired)
                    {
                        return Err(FoldError::RetiredIdentity);
                    }
                }
                if v.label.is_some() != v.label_provenance.is_some()
                    || v.adoption.is_some() != (v.mode == ProvisionMode::Adopt)
                    || v.medium.record_size == 0
                    || v.pool.as_ref().is_some_and(Text::is_empty)
                {
                    return Err(FoldError::InvalidSchema);
                }
                if v.label
                    .as_ref()
                    .is_some_and(|l| l.is_empty() || self.labels.contains_key(l))
                {
                    return Err(FoldError::LabelHeld);
                }
                if let Some(s) = &v.parity_scheme {
                    if s.id.as_str().len() > 4096 || s.validate().is_err() {
                        return Err(FoldError::InvalidSchema);
                    }
                }
                if let Some(a) = &v.adoption {
                    if v.label
                        .as_ref()
                        .is_some_and(|l| l.as_str().trim() != l.as_str())
                        || !v.medium.record_size.is_multiple_of(512)
                        || a.operation != a.idempotency_key
                        || !a.identity_only
                        || a.bootstrap_drive_compression
                        || a.configured_drive_compression
                        || v.pool.is_none()
                        || v.endpoint.is_none()
                        || a.drive_serial.is_empty()
                        || v.tape.uuid[6] >> 4 != 4
                        || v.tape.uuid[8] & 0xc0 != 0x80
                    {
                        return Err(FoldError::InvalidSchema);
                    }
                    if (a.physical_tail == PhysicalTail::ExactBootstrapFilemarkEod)
                        != (a.state == AdoptedState::Ready)
                    {
                        return Err(FoldError::InvalidSchema);
                    }
                    match v.endpoint.as_ref().unwrap() {
                        DriveKey::Bay(e) => {
                            if e.library_serial.is_empty()
                                || a.library_revision.as_ref().is_none_or(Text::is_empty)
                                || a.home_slot.is_none()
                            {
                                return Err(FoldError::InvalidSchema);
                            }
                        }
                        DriveKey::Standalone(e) => {
                            if e.drive_serial != a.drive_serial
                                || a.library_revision.is_some()
                                || a.home_slot.is_some()
                            {
                                return Err(FoldError::InvalidSchema);
                            }
                        }
                    }
                }
            }
            AssignPool(v) => {
                generation(&v.change, self.tape(v.tape)?.assignment_generation)?;
                if v.pool.as_ref().is_some_and(Text::is_empty) {
                    return Err(FoldError::InvalidSchema);
                }
                if v.source != AssignmentSource::Rule {
                    if let Some(c) = v.configuration {
                        let record = self
                            .historical(c, history)?
                            .ok_or(FoldError::WrongCounterpart)?;
                        if record.body.tag() != Tag::ConfigurationAccepted {
                            return Err(FoldError::WrongCounterpart);
                        }
                    }
                }
            }
            Label(v) => {
                generation(&v.change, self.tape(v.tape)?.label_generation)?;
                if v.observed_bot_uuid.is_some_and(|id| id != v.tape.uuid) {
                    return Err(FoldError::ObservedIdentity);
                }
                if v.label.as_ref().is_some_and(LabelText::is_empty) {
                    return Err(FoldError::InvalidSchema);
                }
                let holder = v.label.as_ref().and_then(|l| {
                    self.labels
                        .get(l)
                        .filter(|id| **id != v.tape.uuid)
                        .and_then(|id| self.tapes.get(id))
                });
                match (holder, &v.prior_holder) {
                    (Some(t), Some(p)) if t.tape == p.tape => {
                        generation(&p.change, t.label_generation)?
                    }
                    (Some(_), None) => return Err(FoldError::LabelHeld),
                    (None, None) => {}
                    _ => return Err(FoldError::PriorHolder),
                }
            }
            Retire(v) => {
                if self.tape(v.tape)?.assignment_generation != v.assignment_generation {
                    return Err(FoldError::GenerationMismatch);
                }
            }
            MediumSpecSet(v) => {
                let old = self.tape(v.tape)?;
                generation(&v.change, old.media_revision)?;
                for (field, changed) in [
                    (
                        "record_size",
                        v.medium.record_size != old.medium.record_size,
                    ),
                    ("rerecord", v.medium.rerecord != old.medium.rerecord),
                    (
                        "lifecycle_ops",
                        v.medium.lifecycle_ops != old.medium.lifecycle_ops,
                    ),
                ] {
                    if changed {
                        return Err(FoldError::ImmutableMediumField { field });
                    }
                }
                match (old.medium.family, v.medium.family) {
                    (MediumFamily::Lto(old_gen), MediumFamily::Lto(new_gen)) => {
                        if old_gen != new_gen && old.ever_measured {
                            return Err(FoldError::MediumFamilyChanged);
                        }
                    }
                    (MediumFamily::Disk, MediumFamily::Disk) => {}
                    (MediumFamily::Lto(_), MediumFamily::Disk)
                    | (MediumFamily::Disk, MediumFamily::Lto(_)) => {
                        return Err(FoldError::MediumFamilyChanged);
                    }
                }
                if v.medium.usable_logical_bytes > old.medium.usable_logical_bytes
                    && v.medium.source != GenerationSource::Measured
                {
                    return Err(FoldError::CapacityIncreaseRequiresMeasurement);
                }
            }
            AcceptUnprotectedFinalization(v) => {
                if v.w >= v.t {
                    return Err(FoldError::InvalidSchema);
                }
            }
            FenceRaised(v) => {
                if !matches!(
                    (&v.kind, &v.scope),
                    (FenceKind::Write, FenceScope::Tape(_))
                        | (FenceKind::DriveFault, FenceScope::Drive(_))
                ) {
                    return Err(FoldError::FenceScopeMismatch);
                }
            }
            FenceReleased(v) => {
                if v.evidence.is_empty() {
                    return Err(FoldError::InvalidSchema);
                }
                if let Some(held) = self.fences.get(&v.fence) {
                    if let Body::FenceRaised(raised) = &held.record.body {
                        release_scope(&raised.scope)?;
                        if !v.detail.0.is_empty()
                            && (!v.forced || raised.cause != FenceCause::NativeLogBehindProjection)
                        {
                            return Err(FoldError::InvalidSchema);
                        }
                    }
                }
                if !self.fences.contains_key(&v.fence) {
                    let retained = self.at_sequence(v.fence);
                    let historical;
                    let record = if let Some(h) = retained {
                        Some(&h.record)
                    } else {
                        historical = history.lookup_fence(v.fence)?;
                        historical.as_ref()
                    };
                    return Err(match record.map(|r| &r.body) {
                        Some(Body::FenceRaised(_)) => FoldError::FenceAlreadyReleased,
                        Some(_) => FoldError::WrongCounterpart,
                        None => FoldError::ReleaseWithoutFence,
                    });
                }
            }
            MotionIntent(v) => {
                if self.commands.contains_key(&v.command) {
                    return Err(FoldError::ConflictingDuplicate);
                }
                if let Motion::Load(l) = &v.motion {
                    let expected = v
                        .parent_operation
                        .map(|op| {
                            self.loads
                                .get(&(op, l.endpoint.clone()))
                                .copied()
                                .unwrap_or(0)
                        })
                        .unwrap_or(0)
                        .checked_add(1)
                        .ok_or(FoldError::LoadAttempt)?;
                    if l.attempt != expected {
                        return Err(FoldError::LoadAttempt);
                    }
                }
            }
            MotionOutcome(v) => {
                let h = self.typed(
                    v.intent,
                    Tag::MotionIntent,
                    FoldError::OutcomeWithoutOpenIntent,
                    history,
                )?;
                if self.uncertain.contains(&h.sequence) {
                    return Err(FoldError::OutcomeWithoutOpenIntent);
                }
                let Body::MotionIntent(i) = &h.record.body else {
                    unreachable!()
                };
                if v.result == MotionResult::Done
                    && !matches!(
                        (&i.motion, &v.confirmation),
                        (
                            Motion::Load(_),
                            Some(Confirmation::Readiness(_) | Confirmation::Inventory(_))
                        ) | (
                            Motion::Move(_) | Motion::Unload(_),
                            Some(Confirmation::Inventory(_))
                        )
                    )
                {
                    return Err(FoldError::Confirmation);
                }
            }
            InventoryReconciled(v) => {
                if v.observation_generation
                    <= self.inventories.get(&v.subject).copied().unwrap_or(0)
                {
                    return Err(FoldError::InventoryGeneration);
                }
                let mut seen = BTreeSet::new();
                for d in &v.resolves.0 {
                    if !seen.insert(d.sequence) {
                        return Err(FoldError::InventoryScope);
                    }
                    let h = self.typed(
                        *d,
                        Tag::MotionIntent,
                        FoldError::OutcomeWithoutOpenIntent,
                        history,
                    )?;
                    let Body::MotionIntent(i) = &h.record.body else {
                        unreachable!()
                    };
                    if !motion_scopes(&i.motion).contains(&inventory_scope(&v.subject)) {
                        return Err(FoldError::InventoryScope);
                    }
                    if !v.complete {
                        let addresses: Vec<_> = match &i.motion {
                            Motion::Move(m) => vec![m.source, m.destination],
                            Motion::Load(l) => match &l.endpoint {
                                DriveKey::Bay(d) => vec![d.bay],
                                _ => vec![],
                            },
                            Motion::Unload(l) => match &l.endpoint {
                                DriveKey::Bay(d) => vec![d.bay],
                                _ => vec![],
                            },
                        };
                        if addresses.is_empty()
                            || addresses
                                .iter()
                                .any(|a| !v.elements.0.iter().any(|e| e.address == *a))
                        {
                            return Err(FoldError::InventoryScope);
                        }
                    }
                }
            }
            VerificationProgress(v) => {
                if v.next.part != v.parts_done {
                    return Err(FoldError::ImpossibleCursor);
                }
                if let Some(h) = self.last(Tag::VerificationProgress, Scope::Operation(v.operation))
                {
                    let Body::VerificationProgress(old) = &h.record.body else {
                        unreachable!()
                    };
                    if old.at != v.at || old.plan_digest != v.plan_digest || old.modes != v.modes {
                        return Err(FoldError::CoverageChanged);
                    }
                    if v.parts_done <= old.parts_done || v.next <= old.next {
                        return Err(FoldError::ProgressRegressed);
                    }
                }
            }
            VerificationVerdict(v) => {
                if v.damaged.0.iter().any(|d| d.start_lba >= d.end_lba)
                    || v.unprotected.as_ref().is_some_and(|u| u.w >= u.t)
                {
                    return Err(FoldError::InvalidSchema);
                }
                let p = self.last(Tag::VerificationProgress, Scope::Operation(v.operation));
                if p.map(|h| h.record.reference(h.sequence)) != v.progress {
                    return Err(FoldError::VerdictWithoutCoverage);
                }
                if let Some(h) = p {
                    let Body::VerificationProgress(p) = &h.record.body else {
                        unreachable!()
                    };
                    if v.at.as_ref() != Some(&p.at) || p.modes != v.modes {
                        return Err(FoldError::VerdictWithoutCoverage);
                    }
                }
            }
            ManualFinalizationAccepted(v) => {
                if v.assignment_generation != self.tape(v.at.tape)?.assignment_generation {
                    return Err(FoldError::GenerationMismatch);
                }
            }
            ConfigurationAccepted(v) => {
                let old = self
                    .last(Tag::ConfigurationAccepted, Scope::Config)
                    .map(|h| {
                        if let Body::ConfigurationAccepted(c) = &h.record.body {
                            c.generation
                        } else {
                            unreachable!()
                        }
                    })
                    .unwrap_or(0);
                if old.checked_add(1) != Some(v.generation) {
                    return Err(FoldError::GenerationMismatch);
                }
                if codec::configuration_digest(&v.definitions)
                    .map_err(|_| FoldError::InvalidSchema)?
                    != v.config_digest
                {
                    return Err(FoldError::ConfigDigest);
                }
            }
            ReadinessObserved(v) => {
                if let Some(d) = v.previous {
                    if self.historical(d, history)?.is_some_and(|h| {
                        matches!(&h.body, Body::ReadinessObserved(old)
                            if old.operation == v.operation && old.result != ReadinessResult::Pending)
                    }) {
                        return Err(FoldError::OperationState);
                    }
                }
                let h = self.chain(
                    Tag::ReadinessObserved,
                    Scope::Operation(v.operation),
                    v.previous,
                )?;
                if h.is_some_and(|h|matches!(&h.record.body,Body::ReadinessObserved(r) if r.result!=ReadinessResult::Pending)){return Err(FoldError::OperationState);}
                if v.ready_generation <= self.readiness.get(&v.endpoint).copied().unwrap_or(0) {
                    return Err(FoldError::GenerationMismatch);
                }
                if let Some(d) = v.load_intent {
                    let h = self
                        .historical(d, history)?
                        .ok_or(FoldError::WrongCounterpart)?;
                    if !matches!(&h.body,Body::MotionIntent(i) if matches!(&i.motion,Motion::Load(l) if l.endpoint==v.endpoint))
                    {
                        return Err(FoldError::WrongCounterpart);
                    }
                }
            }
            DriveObserved(_) => {}
            DriveDecision(v) => {
                let (g, retired) = self
                    .drives
                    .get(&v.drive_uuid)
                    .ok_or(FoldError::UnknownDrive)?;
                generation(&v.change, *g)?;
                if matches!(v.decision, Stewardship::Retire(_)) && *retired
                    || matches!(v.decision, Stewardship::Reinstate(_)) && !*retired
                {
                    return Err(FoldError::DriveState);
                }
            }
            DriveHealthObserved(v) => {
                if !self.drives.contains_key(&v.drive_uuid) {
                    return Err(FoldError::UnknownDrive);
                }
            }
            CleaningRecorded(v) => {
                if matches!(
                    v.action,
                    CleaningAction::Register(_)
                        | CleaningAction::Expire(_)
                        | CleaningAction::Reject(_)
                ) {
                    if v.cartridge.is_none() || v.previous.is_some() {
                        return Err(FoldError::Chain);
                    }
                } else {
                    let last = self.chain(
                        Tag::CleaningRecorded,
                        Scope::CleaningRun(v.run_id.clone()),
                        v.previous,
                    )?;
                    // An omitted cartridge inherits the existing run binding in SQL.
                    // Do not let that spelling bypass permanent rejection or expiry.
                    let bound_cartridge = v
                        .cartridge
                        .or_else(|| self.cleaning_run_cartridges.get(&v.run_id).copied());
                    if bound_cartridge
                        .and_then(|t| self.cleaning_cartridges.get(&t.uuid))
                        .is_some_and(|c| c.rejected || c.expired)
                        && !matches!(
                            v.action,
                            CleaningAction::Finish(Finish {
                                verified_credit: false,
                                ..
                            })
                        )
                    {
                        return Err(FoldError::RetiredIdentity);
                    }
                    let begins = matches!(v.action, CleaningAction::Begin(_));
                    if begins {
                        if last.is_some() { return Err(FoldError::Chain); }
                    } else if last.is_none_or(|h| matches!(&h.record.body,
                        Body::CleaningRecorded(c) if matches!(c.action,
                            CleaningAction::Finish(_) | CleaningAction::Expire(_) | CleaningAction::Register(_)))) {
                        return Err(FoldError::Chain);
                    }
                    // Cartridge selection can occur after Begin. Check the new resource
                    // binding at every active step, excluding this run's own prior state.
                    if !matches!(v.action, CleaningAction::Finish(_))
                        && (v
                            .drive_uuid
                            .and_then(|id| self.cleaning_drives.get(&id))
                            .is_some_and(|run| *run != v.run_id)
                            || v.cartridge
                                .and_then(|t| self.cleaning_tapes.get(&t.uuid))
                                .is_some_and(|run| *run != v.run_id))
                    {
                        return Err(FoldError::CleaningBusy);
                    }
                }
            }
            AlarmChanged(v) => {
                let old = self
                    .last(Tag::AlarmChanged, Scope::Alarm(v.condition_key.clone()))
                    .map(|h| {
                        if let Body::AlarmChanged(a) = &h.record.body {
                            a
                        } else {
                            unreachable!()
                        }
                    });
                generation(&v.change, old.map(|a| a.change.next).unwrap_or(0))?;
                let raised = old.is_some_and(|a| !matches!(a.action, AlarmAction::Clear(_)));
                match &v.action {
                    AlarmAction::Raise(_) if raised => return Err(FoldError::AlarmState),
                    AlarmAction::Acknowledge(_)
                        if !raised || self.alarm_acknowledgers.contains_key(&v.condition_key) =>
                    {
                        return Err(FoldError::AlarmState)
                    }
                    AlarmAction::Acknowledge(_) if r.envelope.actor == AuditActor::System => {
                        return Err(FoldError::SystemAcknowledgement)
                    }
                    AlarmAction::Refresh(_) if !raised => return Err(FoldError::AlarmState),
                    AlarmAction::Clear(_) if !raised => return Err(FoldError::AlarmState),
                    _ => {}
                }
                if let AlarmAction::Refresh(a) = &v.action {
                    if let Some(d) = a.cause {
                        self.historical(d, history)?
                            .ok_or(FoldError::WrongCounterpart)?;
                    }
                }
                if let Some(d) = v.cause {
                    self.historical(d, history)?
                        .ok_or(FoldError::WrongCounterpart)?;
                }
            }
            OperationRecorded(v) => {
                if let Some(previous) = v.previous {
                    if self.historical(previous, history)?.is_some_and(|r| matches!(&r.body, Body::OperationRecorded(old) if old.operation == v.operation && terminal(&old.event))) {
                        return Err(FoldError::OperationState);
                    }
                }
                let old = self.chain(
                    Tag::OperationRecorded,
                    Scope::Operation(v.operation),
                    v.previous,
                )?;
                if old.is_some_and(|h|matches!(&h.record.body,Body::OperationRecorded(o) if terminal(&o.event)||o.kind!=v.kind)){return Err(FoldError::OperationState);}
                match &v.event {
                    OperationEvent::Accepted(a) => {
                        if old.is_some() { return Err(FoldError::OperationState); }
                        if let Some(k) = a.idempotency_key {
                            if let Some(entry) = self.idempotency.get(&idempotency_digest(&a.actor_fingerprint, v.kind, k)) {
                                if *entry != (v.operation, a.request_fingerprint) {
                                    return Err(FoldError::ConflictingDuplicate);
                                }
                                return Err(FoldError::ClosedOperationReplay { operation: v.operation });
                            }
                        }
                    }
                    OperationEvent::Started(_) => {
                        if old.is_some_and(|h| !matches!(&h.record.body,
                            Body::OperationRecorded(o) if matches!(o.event, OperationEvent::Accepted(_)))) {
                            return Err(FoldError::OperationState);
                        }
                    }
                    _ => { if old.is_none() { return Err(FoldError::OperationState); } }
                }
            }
            SessionObserved(v) => {
                let old = self.chain(
                    Tag::SessionObserved,
                    Scope::Operation(v.session_id),
                    v.previous,
                )?;
                if v.event==SessionEvent::Opened{if old.is_some(){return Err(FoldError::SessionState);}}else if old.is_none_or(|h|!matches!(&h.record.body,Body::SessionObserved(s) if s.event==SessionEvent::Opened && s.kind==v.kind && s.tape==v.tape && s.endpoint==v.endpoint)){return Err(FoldError::SessionState);}
            }
            WrapMapCached(v) => {
                if v.calibration_generation < self.tape(v.tape)?.calibration_generation {
                    return Err(FoldError::GenerationMismatch);
                }
                if let WrapAction::Cache(c) = &v.action {
                    let digest: Hash = Sha256::digest(&c.descriptors.0).into();
                    if digest != c.digest {
                        return Err(FoldError::CacheDigest);
                    }
                }
            }
            RecoveryRecorded(v) => {
                if let Some(d) = v.prior {
                    let record = self
                        .historical(d, history)?
                        .ok_or(FoldError::WrongCounterpart)?;
                    if record.body.tag() != Tag::RecoveryRecorded {
                        return Err(FoldError::WrongCounterpart);
                    }
                }
            }
            Diagnostic(v) => {
                if v.event == DiagnosticEvent::RefusalRecorded {
                    let code = v.detail.0.iter().find(|e| e.0.as_str() == "code");
                    let name = v.detail.0.iter().find(|e| e.0.as_str() == "name");
                    if !matches!(code, Some(DiagnosticEntry(_, DiagnosticValue::Unsigned(n)))
                        if u32::try_from(*n).ok().and_then(RefusalCode::from_u32).is_some())
                        || !matches!(name, Some(DiagnosticEntry(_, DiagnosticValue::Text(_))))
                    {
                        return Err(FoldError::InvalidSchema);
                    }
                }
            }
            SupersedeTail(_) => {}
        }
        Ok(Validated::Apply)
    }
    /// Validate and apply one contiguous record; unresolved native proofs are refused.
    pub fn apply(
        &mut self,
        sequence: u64,
        record: Record,
        receipt: Receipt,
        view: &dyn TapeView,
        history: &dyn LibraryHistory,
    ) -> Result<Validated, FoldError> {
        if sequence != self.received_through + 1 {
            return Err(FoldError::Sequence);
        }
        let result = self.validate(&record, view, history)?;
        let at = record.body.at().cloned();
        let floor_at = at.clone().or_else(|| match &record.body {
            Body::FenceRaised(v) => v.at.clone(),
            _ => None,
        });
        match &result {
            Validated::Apply => self.apply_validated(sequence, record, receipt, view)?,
            Validated::Duplicate(_) => {
                self.duplicates += 1;
                self.non_applications += 1;
            }
        }
        self.note_reference(at.as_ref());
        self.note_floor(floor_at.as_ref(), sequence);
        self.received_through = sequence;
        self.watermark();
        Ok(result)
    }
    pub(super) fn apply_validated(
        &mut self,
        sequence: u64,
        record: Record,
        receipt: Receipt,
        view: &dyn TapeView,
    ) -> Result<(), FoldError> {
        self.apply_validated_observed(sequence, record, receipt, view, &mut |_| {})
    }
    /// Report successful applications synchronously, including the prefix before an error.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_validated_observed(
        &mut self,
        sequence: u64,
        record: Record,
        receipt: Receipt,
        view: &dyn TapeView,
        applied: &mut dyn FnMut(u64),
    ) -> Result<(), FoldError> {
        // Fence scopes have no historical references. Recompute exactly the scope
        // validated before publication; no mutation can intervene under the writer lock.
        let deferred_scope = if matches!(record.body, Body::FenceRaised(_)) {
            let scope = self.fence_scopes(&record.body);
            self.blocked(&scope).then_some(scope)
        } else {
            None
        };
        let at = record.body.at().cloned();
        let floor_at = at.clone().or_else(|| match &record.body {
            Body::FenceRaised(v) => v.at.clone(),
            _ => None,
        });
        if let Some(scope) = deferred_scope {
            self.defer_fence(sequence, record, receipt, scope, view, applied)?;
        } else {
            self.commit_observed(sequence, record, receipt, view, applied)?;
        }
        self.note_reference(at.as_ref());
        self.note_floor(floor_at.as_ref(), sequence);
        self.received_through = sequence;
        self.watermark();
        Ok(())
    }
    // Observing after commit keeps the frozen transition and all its early returns intact.
    fn commit_observed(
        &mut self,
        sequence: u64,
        record: Record,
        receipt: Receipt,
        view: &dyn TapeView,
        applied: &mut dyn FnMut(u64),
    ) -> Result<(), FoldError> {
        self.commit(sequence, record, receipt, view)?;
        applied(sequence);
        Ok(())
    }
    fn commit(
        &mut self,
        sequence: u64,
        record: Record,
        receipt: Receipt,
        view: &dyn TapeView,
    ) -> Result<(), FoldError> {
        let b = &record.body;
        use Body::*;
        match b {
            Reprovision(v) => {
                if let Some(l) = &v.label {
                    self.labels.insert(l.clone(), v.tape.uuid);
                }
                self.tapes.insert(
                    v.tape.uuid,
                    TapeState {
                        tape: v.tape,
                        retired: false,
                        assignment_generation: 1,
                        label_generation: 1,
                        media_revision: 1,
                        medium: v.medium.clone(),
                        ever_measured: v.medium.source == GenerationSource::Measured,
                        label: v.label.clone(),
                        pool: v.pool.clone(),
                        pool_source: v.pool_source.clone(),
                        calibration_generation: 0,
                        wrap_digest: None,
                    },
                );
            }
            AssignPool(v) => {
                let t = self.tapes.get_mut(&v.tape.uuid).unwrap();
                t.assignment_generation = v.change.next;
                t.pool = v.pool.clone();
                t.pool_source = match v.source {
                    AssignmentSource::Operator => Some(PoolSource::Operator(Unit)),
                    // Validation refuses a Rule assignment without its configuration reference.
                    AssignmentSource::Rule => v.configuration.map(PoolSource::Rule),
                };
            }
            Label(v) => {
                if let Some(l) = &self.tapes[&v.tape.uuid].label {
                    self.labels.remove(l);
                }
                if let Some(l) = &v.label {
                    self.labels.insert(l.clone(), v.tape.uuid);
                }
                if let Some(p) = &v.prior_holder {
                    let t = self.tapes.get_mut(&p.tape.uuid).unwrap();
                    t.label = None;
                    t.label_generation = p.change.next;
                }
                let t = self.tapes.get_mut(&v.tape.uuid).unwrap();
                t.label = v.label.clone();
                t.label_generation = v.change.next;
            }
            Retire(v) => {
                if let Some(l) = &self.tapes[&v.tape.uuid].label {
                    self.labels.remove(l);
                }
                let t = self.tapes.get_mut(&v.tape.uuid).unwrap();
                t.retired = true;
                t.pool = None;
                t.pool_source = None;
                t.label = None;
            }
            MediumSpecSet(v) => {
                let t = self.tapes.get_mut(&v.tape.uuid).unwrap();
                t.media_revision = v.change.next;
                t.ever_measured |= v.medium.source == GenerationSource::Measured;
                t.medium = v.medium.clone();
            }
            FenceRaised(v) => {
                if let Some(at) = &v.at {
                    if !matches!(view.lookup(at)?,TapeLookup::Present{digest,..} if digest==at.record_digest)
                    {
                        self.unresolved_fence_evidence.insert(sequence);
                    }
                }
                if let FenceScope::Tape(tape) = &v.scope {
                    self.tape_fences
                        .entry(tape.uuid)
                        .or_default()
                        .insert(sequence);
                }
                self.fences.insert(
                    sequence,
                    Held {
                        sequence,
                        record: record.clone(),
                        receipt: receipt.clone(),
                    },
                );
            }
            FenceReleased(v) => {
                let raised = self
                    .fences
                    .get(&v.fence)
                    .ok_or(FoldError::ReleaseWithoutFence)?;
                let Body::FenceRaised(raised) = &raised.record.body else {
                    return Err(FoldError::WrongCounterpart);
                };
                let scope = release_scope(&raised.scope)?;
                if v.forced && raised.cause == FenceCause::NativeLogBehindProjection {
                    if let FenceScope::Tape(tape) = &raised.scope {
                        self.forced_observation.insert(tape.uuid, v.detail.clone());
                    }
                }
                if let FenceScope::Tape(tape) = &raised.scope {
                    if let Some(sequences) = self.tape_fences.get_mut(&tape.uuid) {
                        sequences.remove(&v.fence);
                        if sequences.is_empty() {
                            self.tape_fences.remove(&tape.uuid);
                        }
                    }
                }
                self.fences.remove(&v.fence);
                self.unresolved_fence_evidence.remove(&v.fence);
                self.fence_liveness_failures.remove(&v.fence);
                self.insert_latest(
                    (Tag::FenceReleased, scope, None),
                    Held {
                        sequence,
                        record: record.clone(),
                        receipt: receipt.clone(),
                    },
                );
            }
            MotionIntent(v) => {
                if let (Some(op), Motion::Load(l)) = (v.parent_operation, &v.motion) {
                    self.loads.insert((op, l.endpoint.clone()), l.attempt);
                    self.load_endpoints
                        .entry(op)
                        .or_default()
                        .insert(l.endpoint.clone());
                }
                self.commands.insert(v.command, sequence);
                for scope in b.scopes() {
                    *self.intent_scopes.entry(scope).or_default() += 1;
                }
                self.intents.insert(
                    sequence,
                    Held {
                        sequence,
                        record: record.clone(),
                        receipt: receipt.clone(),
                    },
                );
            }
            MotionOutcome(v) => {
                if v.result == MotionResult::Unknown {
                    self.uncertain.insert(v.intent.sequence);
                } else {
                    self.remove_intent(v.intent.sequence);
                }
            }
            InventoryReconciled(v) => {
                self.inventories
                    .insert(v.subject.clone(), v.observation_generation);
                for d in &v.resolves.0 {
                    self.remove_intent(d.sequence);
                    self.uncertain.remove(&d.sequence);
                }
            }
            ReadinessObserved(v) => {
                self.readiness
                    .insert(v.endpoint.clone(), v.ready_generation);
                if v.result != ReadinessResult::Pending {
                    self.close_loads(v.operation);
                }
            }
            DriveObserved(v) => {
                self.drives.entry(v.drive_uuid).or_insert((1, false));
            }
            DriveDecision(v) => {
                let d = self.drives.get_mut(&v.drive_uuid).unwrap();
                d.0 = v.change.next;
                match v.decision {
                    Stewardship::Retire(_) => d.1 = true,
                    Stewardship::Reinstate(_) => d.1 = false,
                    _ => {}
                }
            }
            OperationRecorded(v) => {
                if let OperationEvent::Accepted(a) = &v.event {
                    if let Some(k) = a.idempotency_key {
                        self.idempotency.insert(
                            idempotency_digest(&a.actor_fingerprint, v.kind, k),
                            (v.operation, a.request_fingerprint),
                        );
                    }
                }
            }
            WrapMapCached(v) => {
                self.tapes
                    .get_mut(&v.tape.uuid)
                    .unwrap()
                    .calibration_generation = v.calibration_generation;
                self.tapes.get_mut(&v.tape.uuid).unwrap().wrap_digest = match &v.action {
                    WrapAction::Cache(c) => Some(c.digest),
                    _ => None,
                };
            }
            AlarmChanged(v) => match &v.action {
                AlarmAction::Acknowledge(_) => {
                    self.alarm_acknowledgers
                        .insert(v.condition_key.clone(), record.envelope.actor.clone());
                }
                AlarmAction::Raise(_) | AlarmAction::Clear(_) => {
                    self.alarm_acknowledgers.remove(&v.condition_key);
                }
                AlarmAction::Refresh(_) => {}
            },
            CleaningRecorded(v) if matches!(v.action, CleaningAction::Register(_)) => {
                let CleaningAction::Register(a) = &v.action else {
                    unreachable!()
                };
                self.cleaning_cartridges.insert(
                    v.cartridge.unwrap().uuid,
                    CleaningCartridgeState {
                        barcode: a.barcode.clone(),
                        home_slot: a.home_slot,
                        expired: false,
                        rejected: false,
                    },
                );
            }
            CleaningRecorded(v) if matches!(v.action, CleaningAction::Reject(_)) => {
                self.cleaning_cartridges
                    .get_mut(&v.cartridge.unwrap().uuid)
                    .unwrap()
                    .rejected = true;
            }
            CleaningRecorded(v) if matches!(v.action, CleaningAction::Expire(_)) => {
                if let Some(c) = v
                    .cartridge
                    .and_then(|t| self.cleaning_cartridges.get_mut(&t.uuid))
                {
                    c.expired = true;
                }
            }
            CleaningRecorded(v)
                if !matches!(
                    v.action,
                    CleaningAction::Register(_)
                        | CleaningAction::Expire(_)
                        | CleaningAction::Reject(_)
                ) =>
            {
                let key = (
                    Tag::CleaningRecorded,
                    Scope::CleaningRun(v.run_id.clone()),
                    None,
                );
                if let Some(h) = self.latest.get(&key) {
                    if let Body::CleaningRecorded(old) = &h.record.body {
                        if let Some(id) = old.drive_uuid {
                            self.cleaning_drives.remove(&id);
                        }
                    }
                }
                if let CleaningAction::Advance(Advance {
                    home_slot: Some(home),
                    ..
                }) = &v.action
                {
                    self.cleaning_run_homes.insert(v.run_id.clone(), *home);
                }
                let old_cartridge = self.cleaning_run_cartridges.remove(&v.run_id);
                if let Some(t) = old_cartridge {
                    self.cleaning_tapes.remove(&t.uuid);
                }
                if !matches!(
                    v.action,
                    CleaningAction::Finish(_)
                        | CleaningAction::Register(_)
                        | CleaningAction::Expire(_)
                ) {
                    if let Some(id) = v.drive_uuid {
                        self.cleaning_drives.insert(id, v.run_id.clone());
                    }
                    if let Some(t) = v.cartridge.or(old_cartridge) {
                        if let Some(home) = self.cleaning_run_homes.get(&v.run_id) {
                            if let Some(c) = self.cleaning_cartridges.get_mut(&t.uuid) {
                                c.home_slot = *home;
                            }
                        }
                        self.cleaning_tapes.insert(t.uuid, v.run_id.clone());
                        self.cleaning_run_cartridges.insert(v.run_id.clone(), t);
                    }
                } else {
                    self.cleaning_run_homes.remove(&v.run_id);
                }
            }
            _ => {}
        }
        match b {
            Body::SupersedeTail(v) => {
                let key = (v.at.tape, v.session_id);
                self.supersede_sequences.insert(sequence, key);
                self.supersedes.insert(
                    key,
                    Held {
                        sequence,
                        record,
                        receipt,
                    },
                );
                return Ok(());
            }
            Body::AcceptUnprotectedFinalization(v) => {
                self.unprotected.insert(
                    (v.at.tape, v.operation),
                    Held {
                        sequence,
                        record,
                        receipt,
                    },
                );
                return Ok(());
            }
            _ => {}
        }
        if let Body::OperationRecorded(v) = b {
            if terminal(&v.event) {
                self.close_operation(v.operation);
                return Ok(());
            }
        }
        if let Some(slot) = b.slot() {
            let key = (b.tag(), slot, b.slot_tape());
            if matches!(b,SessionObserved(v) if v.event!=SessionEvent::Opened)
                || matches!(b, ReadinessObserved(v) if v.result != ReadinessResult::Pending)
                || matches!(b, CleaningRecorded(v) if matches!(v.action, CleaningAction::Finish(_)))
            {
                self.remove_latest(&key);
            } else {
                self.insert_latest(
                    key,
                    Held {
                        sequence,
                        record,
                        receipt,
                    },
                );
            }
        }
        Ok(())
    }
    fn scopes_for(
        &self,
        b: &Body,
        history: &dyn LibraryHistory,
    ) -> Result<BTreeSet<Scope>, FoldError> {
        let mut s = self.fence_scopes(b);
        for d in b.refs() {
            // Rule configurations use only the retained latest configuration.
            if matches!(b, Body::AssignPool(v) if v.source == AssignmentSource::Rule)
                || matches!(b, Body::Reprovision(v) if matches!(v.pool_source, Some(PoolSource::Rule(_))))
            {
                s.insert(Scope::Config);
            } else if let Some(h) = self.historical(d, history)? {
                s.extend(h.body.scopes());
            }
        }
        if let Body::FenceReleased(v) = b {
            if let Some(h) = self.fences.get(&v.fence) {
                if let Body::FenceRaised(raised) = &h.record.body {
                    s.insert(release_scope(&raised.scope)?);
                }
            }
        }
        Ok(s)
    }
    /// A fence orders only its scope-tape liveness check; drive fences never defer.
    /// Other records retain their resource scopes and current labels.
    fn fence_scopes(&self, b: &Body) -> BTreeSet<Scope> {
        if let Body::FenceRaised(v) = b {
            return match &v.scope {
                FenceScope::Tape(t) => BTreeSet::from([Scope::Tape(t.uuid)]),
                FenceScope::Drive(_) | FenceScope::Model { .. } => BTreeSet::new(),
            };
        }
        // An unreferenced verdict orders only on its tape and operation, never labels.
        if matches!(b, Body::VerificationVerdict(v) if v.at.is_none()) {
            return b.scopes();
        }
        let mut scopes = b.scopes();
        for tape in b.tapes() {
            if let Some(label) = self.tapes.get(&tape.uuid).and_then(|t| t.label.as_ref()) {
                scopes.insert(Scope::Label(label.clone()));
            }
        }
        scopes
    }
    fn blocked(&self, scopes: &BTreeSet<Scope>) -> bool {
        self.pending.iter().any(|p| !p.scope.is_disjoint(scopes))
    }
    fn validate_deferred_fence(
        &self,
        record: &Record,
        scope: &BTreeSet<Scope>,
    ) -> Result<(), FoldError> {
        let Body::FenceRaised(v) = &record.body else {
            unreachable!()
        };
        if !matches!(
            (&v.kind, &v.scope),
            (FenceKind::Write, FenceScope::Tape(_)) | (FenceKind::DriveFault, FenceScope::Drive(_))
        ) {
            return Err(FoldError::FenceScopeMismatch);
        }
        self.check_pending_capacity(record, scope)
    }
    /// Restrict immediately, retaining the same queued liveness check for append and replay.
    fn defer_fence(
        &mut self,
        sequence: u64,
        record: Record,
        receipt: Receipt,
        scope: BTreeSet<Scope>,
        view: &dyn TapeView,
        applied: &mut dyn FnMut(u64),
    ) -> Result<(), FoldError> {
        self.validate_deferred_fence(&record, &scope)?;
        self.hold(sequence, record.clone(), receipt.clone(), scope)?;
        self.pending.last_mut().unwrap().deferred_fence = true;
        self.commit_observed(sequence, record, receipt, view, applied)?;
        Ok(())
    }
    /// Receive one frame during replay; only native contradictions are isolated.
    pub fn receive(
        &mut self,
        sequence: u64,
        record: Record,
        receipt: Receipt,
        view: &dyn TapeView,
        history: &dyn LibraryHistory,
    ) -> Result<(), FoldError> {
        self.receive_observed(sequence, record, receipt, view, history, &mut |_| {})
    }
    /// Report successful applications synchronously, including the prefix before an error.
    #[allow(clippy::too_many_arguments)]
    pub fn receive_observed(
        &mut self,
        sequence: u64,
        record: Record,
        receipt: Receipt,
        view: &dyn TapeView,
        history: &dyn LibraryHistory,
        applied: &mut dyn FnMut(u64),
    ) -> Result<(), FoldError> {
        if sequence != self.received_through + 1 {
            return Err(FoldError::Sequence);
        }
        self.validate_shape(sequence, &record)?;
        let history = &HistoryPrefix {
            history,
            before: sequence,
        };
        self.check_dependent_identity(&record.body)?;
        self.check_repair_floor(&record.body)?;
        let scope = self.scopes_for(&record.body, history)?;
        let blocked = self.blocked(&scope);
        let at = record.body.at().cloned();
        let floor_at = at.clone().or_else(|| match &record.body {
            Body::FenceRaised(v) => v.at.clone(),
            _ => None,
        });
        // D2/D4: native evidence is decided even behind an unrelated scope wait.
        let validation = if blocked {
            self.check_reference(&record.body, view)
                .map(|_| Validated::Apply)
        } else {
            self.validate_at(sequence, &record, view, history)
        };
        match validation {
            Err(FoldError::TapeContradiction) => {
                self.exclude(at.as_ref().unwrap());
                self.non_applications += 1;
            }
            Err(FoldError::Unresolved) => self.hold(sequence, record, receipt, scope)?,
            Err(e) => return Err(e),
            Ok(_) if blocked && matches!(record.body, Body::FenceRaised(_)) => {
                self.defer_fence(sequence, record, receipt, scope, view, applied)?;
            }
            Ok(_) if blocked => self.hold(sequence, record, receipt, scope)?,
            Ok(Validated::Apply) => {
                self.commit_observed(sequence, record, receipt, view, applied)?
            }
            Ok(Validated::Duplicate(_)) => {
                self.duplicates += 1;
                self.non_applications += 1;
            }
        }
        self.note_reference(at.as_ref());
        self.note_floor(floor_at.as_ref(), sequence);
        self.received_through = sequence;
        self.watermark();
        Ok(())
    }
    /// Current total canonical pending bytes (including envelopes).
    pub fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }
    fn pending_encoding(record: &Record) -> Result<Box<[u8]>, FoldError> {
        let mut bytes = crate::log_frame::Limited {
            bytes: Vec::new(),
            limit: crate::log_frame::MAX_FRAME_BODY - 32,
        };
        wire::encode(record, &mut bytes).map_err(|_| FoldError::InvalidSchema)?;
        Ok(bytes.bytes.into_boxed_slice())
    }
    fn capacity(&self, bytes: usize, scope: &BTreeSet<Scope>) -> Result<(), FoldError> {
        let records = self.pending.len() + 1;
        let bytes = self.pending_bytes + bytes;
        if records > MAX_PENDING_RECORDS || bytes > MAX_PENDING_BYTES {
            return Err(FoldError::PendingLimitExceeded {
                scope: scope.clone(),
                records,
                bytes,
            });
        }
        Ok(())
    }
    fn check_pending_capacity(
        &self,
        record: &Record,
        scope: &BTreeSet<Scope>,
    ) -> Result<(), FoldError> {
        self.capacity(Self::pending_encoding(record)?.len(), scope)
    }
    fn hold(
        &mut self,
        sequence: u64,
        record: Record,
        receipt: Receipt,
        scope: BTreeSet<Scope>,
    ) -> Result<(), FoldError> {
        let bytes = Self::pending_encoding(&record)?;
        self.capacity(bytes.len(), &scope)?;
        self.pending_bytes += bytes.len();
        self.pending.push(PendingRecord {
            sequence,
            tag: record.body.tag(),
            key: record.body.at().cloned(),
            scope,
            bytes,
            receipt,
            deferred_fence: false,
        });
        Ok(())
    }
    fn watermark(&mut self) {
        self.applied_through = self
            .pending
            .first()
            .map(|p| p.sequence - 1)
            .unwrap_or(self.received_through);
    }
    /// Retry pending records in order until no additional record can apply.
    pub fn resolve(
        &mut self,
        view: &dyn TapeView,
        history: &dyn LibraryHistory,
    ) -> Result<(), FoldError> {
        self.resolve_observed(view, history, &mut |_| {})
    }
    /// Report successful applications synchronously, including the prefix before an error.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_observed(
        &mut self,
        view: &dyn TapeView,
        history: &dyn LibraryHistory,
        applied: &mut dyn FnMut(u64),
    ) -> Result<(), FoldError> {
        self.pending_rejection = None;
        loop {
            let before = self.pending.len();
            let pending = std::mem::take(&mut self.pending);
            let mut it = pending.into_iter();
            while let Some(p) = it.next() {
                let record = match wire::decode(p.tag as u16, &p.bytes) {
                    Ok(record) => record,
                    Err(_) => {
                        self.pending_rejection = Some((p.sequence, p.receipt.record_uuid));
                        self.pending.push(p);
                        self.pending.extend(it);
                        self.watermark();
                        return Err(FoldError::InvalidSchema);
                    }
                };
                let blocked = self.blocked(&p.scope);
                let validation = if blocked {
                    self.check_reference(&record.body, view)
                        .map(|_| Validated::Apply)
                } else {
                    self.validate_at(p.sequence, &record, view, history)
                };
                match validation {
                    Ok(_) if blocked => self.pending.push(p),
                    Ok(Validated::Apply) => {
                        if !p.deferred_fence {
                            if let Err(e) = self.commit_observed(
                                p.sequence,
                                record,
                                p.receipt.clone(),
                                view,
                                applied,
                            ) {
                                self.pending_rejection = Some((p.sequence, p.receipt.record_uuid));
                                self.pending.push(p);
                                self.pending.extend(it);
                                self.watermark();
                                return Err(e);
                            }
                        }
                        self.pending_bytes -= p.bytes.len();
                    }
                    Ok(Validated::Duplicate(_)) => {
                        self.pending_bytes -= p.bytes.len();
                        self.duplicates += 1;
                        self.non_applications += u64::from(!p.deferred_fence);
                    }
                    Err(FoldError::Unresolved) => self.pending.push(p),
                    Err(FoldError::TapeContradiction) => {
                        self.pending_bytes -= p.bytes.len();
                        self.exclude(record.body.at().unwrap());
                        self.non_applications += u64::from(!p.deferred_fence);
                    }
                    Err(
                        e @ (FoldError::UnknownTape
                        | FoldError::IncarnationMismatch
                        | FoldError::RetiredIdentity),
                    ) if p.deferred_fence => {
                        self.pending_bytes -= p.bytes.len();
                        if let Body::FenceRaised(FenceRaised {
                            scope: FenceScope::Tape(tape),
                            ..
                        }) = record.body
                        {
                            self.fence_liveness_failures.insert(p.sequence, (tape, e));
                        }
                    }
                    Err(e) => {
                        self.pending_rejection = Some((p.sequence, p.receipt.record_uuid));
                        self.pending.push(p);
                        self.pending.extend(it);
                        self.watermark();
                        return Err(e);
                    }
                }
            }
            self.watermark();
            if self.pending.len() == before {
                break;
            }
        }
        Ok(())
    }
    /// Last accepted configuration, used for exact reset retry without another
    /// source append. It is read from the fold, never from SQL.
    pub fn configuration(&self) -> Option<&ConfigurationAccepted> {
        self.configuration_head().map(|(_, value)| value)
    }
    /// The latest retained configuration with the reference a Rule pool source must carry. This is
    /// the same record the fold compares a Rule reference against.
    pub fn configuration_head(&self) -> Option<(DecisionRef, &ConfigurationAccepted)> {
        let held = self.last(Tag::ConfigurationAccepted, Scope::Config)?;
        match &held.record.body {
            Body::ConfigurationAccepted(value) => {
                Some((held.record.reference(held.sequence), value))
            }
            _ => None,
        }
    }
    /// Whether the active raise of this condition already has an acknowledger.
    pub(crate) fn alarm_acknowledged(&self, condition_key: &str) -> bool {
        self.alarm_acknowledgers
            .contains_key(&Text::from(condition_key))
    }
    /// Open operation identity retained by authenticated operation history.
    /// Closed operations are not eligible to begin new readiness work.
    pub(crate) fn open_operation_kind(&self, operation: Id) -> Option<OperationKind> {
        let held = self.last(Tag::OperationRecorded, Scope::Operation(operation))?;
        match &held.record.body {
            Body::OperationRecorded(value) if !terminal(&value.event) => Some(value.kind),
            _ => None,
        }
    }
    /// Current pending readiness chain; terminal chains are archived in source.
    pub fn readiness_head(&self, operation: Id) -> Option<(DecisionRef, &ReadinessObserved)> {
        let held = self.last(Tag::ReadinessObserved, Scope::Operation(operation))?;
        match &held.record.body {
            Body::ReadinessObserved(value) => Some((
                DecisionRef {
                    sequence: held.sequence,
                    record_uuid: held.record.envelope.record_uuid,
                },
                value,
            )),
            _ => None,
        }
    }
    /// Next physical endpoint readiness generation.
    pub fn next_readiness_generation(&self, endpoint: &DriveKey) -> Result<u64, FoldError> {
        self.readiness
            .get(endpoint)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(FoldError::GenerationMismatch)
    }
    /// Open or Unknown command on an affected physical scope blocks dispatch.
    pub fn motion_requires_reconciliation(&self, motion: &Motion) -> bool {
        motion_scopes(motion)
            .iter()
            .any(|scope| self.inventory_unknown(scope))
    }
    /// Whether an open or Unknown motion intent names exactly this endpoint. Unlike
    /// [`Self::motion_requires_reconciliation`], this ignores the Library scope a bay shares
    /// with its changer, so another bay's motion, or a library-scope MOVE, does not answer yes.
    pub fn endpoint_motion_unresolved(&self, endpoint: &DriveKey) -> bool {
        self.inventory_unknown(&Scope::Endpoint(endpoint.clone()))
    }
    /// Strictly next observation generation for the actual physical subject.
    pub fn next_inventory_generation(&self, subject: &InventorySubject) -> Result<u64, FoldError> {
        self.inventories
            .get(subject)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(FoldError::InventoryGeneration)
    }
    /// Count actual admitted LOAD intents for this operation, never SQL
    /// write-ahead rows. Terminal readiness closes this counter in the fold.
    pub fn readiness_load_attempts(&self, operation: Id) -> Result<u32, FoldError> {
        self.load_endpoints
            .get(&operation)
            .into_iter()
            .flatten()
            .try_fold(0u32, |sum, key| {
                sum.checked_add(
                    self.loads
                        .get(&(operation, key.clone()))
                        .copied()
                        .unwrap_or(0),
                )
                .ok_or(FoldError::Sequence)
            })
    }
    /// Next per-operation LOAD number; independent composers without a parent
    /// each start a new single-command attempt sequence.
    pub fn next_load_attempt(
        &self,
        operation: Option<Id>,
        endpoint: &DriveKey,
    ) -> Result<u32, FoldError> {
        operation
            .and_then(|operation| self.loads.get(&(operation, endpoint.clone())).copied())
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(FoldError::LoadAttempt)
    }
    /// Resolve an active raise. SQL may locate it but never supplies scope or cause.
    pub fn active_fence(&self, sequence: u64) -> Option<&FenceRaised> {
        match &self.fences.get(&sequence)?.record.body {
            Body::FenceRaised(raise) => Some(raise),
            _ => None,
        }
    }

    /// Open fences on one tape, in ascending durable raise sequence order.
    pub fn active_tape_fences(&self, tape: &Id) -> impl Iterator<Item = (u64, &FenceRaised)> {
        self.tape_fences
            .get(tape)
            .into_iter()
            .flatten()
            .map(|sequence| {
                (
                    *sequence,
                    self.active_fence(*sequence)
                        .expect("indexed tape fence is open"),
                )
            })
    }

    /// Open fences, individually identified by their raise sequence.
    pub fn open_fences(&self) -> usize {
        self.fences.len()
    }
    /// Whether a changer or standalone endpoint has unresolved motion.
    pub fn inventory_unknown(&self, scope: &Scope) -> bool {
        self.intent_scopes.contains_key(scope)
    }
    /// Native tape fold's typed crash acknowledgment lookup.
    pub fn supersede(&self, sequence: u64) -> Option<&SupersedeTail> {
        match &self
            .supersedes
            .get(self.supersede_sequences.get(&sequence)?)?
            .record
            .body
        {
            Body::SupersedeTail(v) => Some(v),
            _ => None,
        }
    }
}
fn duplicate_key(a: &Body, b: &Body) -> bool {
    match (a, b) {
        (Body::AssignPool(a), Body::AssignPool(b)) => a.change.expected == b.change.expected,
        (Body::Label(a), Body::Label(b)) => a.change.expected == b.change.expected,
        (Body::MediumSpecSet(a), Body::MediumSpecSet(b)) => a.change.expected == b.change.expected,
        (Body::AlarmChanged(a), Body::AlarmChanged(b)) => a.change.expected == b.change.expected,
        (Body::DriveDecision(a), Body::DriveDecision(b)) => a.change.expected == b.change.expected,
        (Body::OperationRecorded(a), Body::OperationRecorded(b)) => a.previous == b.previous,
        (Body::ReadinessObserved(a), Body::ReadinessObserved(b)) => a.previous == b.previous,
        (Body::CleaningRecorded(a), Body::CleaningRecorded(b)) => {
            !matches!(
                b.action,
                CleaningAction::Register(_) | CleaningAction::Expire(_) | CleaningAction::Reject(_)
            ) && a.previous == b.previous
                && std::mem::discriminant(&a.action) == std::mem::discriminant(&b.action)
        }
        (Body::VerificationProgress(a), Body::VerificationProgress(b)) => {
            a.parts_done == b.parts_done
        }
        (Body::ConfigurationAccepted(a), Body::ConfigurationAccepted(b)) => {
            a.generation == b.generation
        }
        (Body::DriveHealthObserved(a), Body::DriveHealthObserved(b)) => {
            a.session_id.is_some() && a.session_id == b.session_id && a.trigger == b.trigger
        }
        (Body::SupersedeTail(_), Body::SupersedeTail(_))
        | (Body::AcceptUnprotectedFinalization(_), Body::AcceptUnprotectedFinalization(_))
        | (Body::ManualFinalizationAccepted(_), Body::ManualFinalizationAccepted(_))
        | (Body::Reprovision(_), Body::Reprovision(_)) => true,
        _ => false,
    }
}

/// Reconciliation orders and resolves within the observed physical subject only.
fn inventory_scope(subject: &InventorySubject) -> Scope {
    match subject {
        InventorySubject::Library(serial) => Scope::Library(serial.clone()),
        InventorySubject::Standalone(endpoint) => {
            Scope::Endpoint(DriveKey::Standalone(endpoint.clone()))
        }
    }
}

/// Length-prefix the text component to make the lifetime idempotency scope unambiguous.
fn idempotency_digest(actor: &Text, kind: OperationKind, key: Id) -> Hash {
    let mut hash = Sha256::new();
    hash.update((actor.0.len() as u64).to_le_bytes());
    hash.update(actor.0.as_bytes());
    hash.update((kind as u16).to_le_bytes());
    hash.update(key);
    hash.finalize().into()
}

#[cfg(test)]
impl LibraryFold {
    /// Compare authority and lookup indexes, excluding replay diagnostics and probe instrumentation.
    pub(super) fn assert_same_state(&self, other: &Self) {
        assert_eq!(self.cleaning_cartridges, other.cleaning_cartridges);
        macro_rules! same { ($($field:ident),+ $(,)?) => { $(assert_eq!(self.$field, other.$field, stringify!($field));)+ }; }
        same!(
            received_through,
            applied_through,
            duplicates,
            tapes,
            pending,
            fence_liveness_failures,
            pending_bytes,
            latest,
            fences,
            tape_fences,
            forced_observation,
            intents,
            uncertain,
            loads,
            inventories,
            drives,
            readiness,
            idempotency,
            by_sequence,
            operation_slots,
            load_endpoints,
            released,
            labels,
            commands,
            intent_scopes,
            cleaning_drives,
            cleaning_tapes,
            cleaning_run_cartridges,
            cleaning_run_homes,
            alarm_acknowledgers,
            supersedes,
            supersede_sequences,
            unprotected
        );
        // D3/D4: excluded counts and unresolved_fence_evidence are replay diagnostics.
        // FenceRaised.at is evidence, so it may be NotYet live and present on replay.
        let authority = |fold: &Self| {
            fold.contradictions
                .iter()
                .map(|(t, s)| {
                    (
                        t.clone(),
                        (
                            s.minimum_contradiction,
                            s.highest_reference,
                            s.floor,
                            s.floor_digests.clone(),
                        ),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(authority(self), authority(other));
        for sequence in 1..=self.received_through {
            assert_eq!(self.supersede(sequence), other.supersede(sequence));
        }
    }
}

/// Release indexing is deliberately exhaustive: disk work must define Model release semantics.
fn release_scope(scope: &FenceScope) -> Result<Scope, FoldError> {
    match scope {
        FenceScope::Tape(tape) => Ok(Scope::Tape(tape.uuid)),
        FenceScope::Drive(endpoint) => Ok(Scope::Endpoint(endpoint.clone())),
        // disk: decide release scope before activating Model fences.
        FenceScope::Model { .. } => Err(FoldError::ReservedNotYetUsed),
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use crate::library_log::tests::{
        endpoint, evidence, feed, provision, record, release, reserved_fence, History, Present,
    };

    /// Even a future Model raise must not make release panic or silently discard its fence.
    #[test]
    fn model_fence_release_is_a_typed_refusal() {
        let mut f = LibraryFold::default();
        feed(&mut f, provision(1, None), &Present).unwrap();
        let Body::FenceRaised(mut raised) = reserved_fence(false) else {
            unreachable!()
        };
        raised.cause = FenceCause::TransferError;
        feed(&mut f, Body::FenceRaised(raised), &Present).unwrap();
        let held = f.fences.get_mut(&2).unwrap();
        let Body::FenceRaised(raised) = &mut held.record.body else {
            unreachable!()
        };
        raised.scope = FenceScope::Model {
            model: "HC560".into(),
            firmware: "A1".into(),
        };
        let before = f.clone();
        let r = record(release(2), 3);
        let receipt = Receipt {
            sequence: 3,
            record_uuid: r.envelope.record_uuid,
            record_hash: [0; 32],
            synced: true,
        };
        assert_eq!(
            f.validate(&r, &Present, &History::default()).unwrap_err(),
            FoldError::ReservedNotYetUsed
        );
        assert_eq!(
            f.receive(3, r.clone(), receipt.clone(), &Present, &History::default())
                .unwrap_err(),
            FoldError::ReservedNotYetUsed
        );
        assert_eq!(
            f.apply_validated(3, r, receipt, &Present).unwrap_err(),
            FoldError::ReservedNotYetUsed
        );
        f.assert_same_state(&before);
    }

    /// Readiness can terminate without a separate OperationRecorded chain.
    #[test]
    fn terminal_readiness_releases_load_attempt_indexes() {
        let mut fold = LibraryFold::default();
        let endpoint = endpoint("LIBA0001");
        feed(
            &mut fold,
            Body::MotionIntent(MotionIntent {
                command: [1; 16],
                parent_operation: Some([8; 16]),
                motion: Motion::Load(Load {
                    endpoint: endpoint.clone(),
                    attempt: 1,
                }),
                tape: None,
                inventory_generation: None,
                readiness_generation: None,
            }),
            &Present,
        )
        .unwrap();
        assert_eq!(fold.loads.len(), 1);
        let intent = fold.intents.get(&1).unwrap().record.reference(1);
        feed(
            &mut fold,
            Body::MotionOutcome(MotionOutcome {
                intent,
                result: MotionResult::Done,
                evidence: evidence(),
                confirmation: Some(Confirmation::Readiness(1)),
            }),
            &Present,
        )
        .unwrap();
        feed(
            &mut fold,
            Body::ReadinessObserved(ReadinessObserved {
                source_slot: None,
                run: None,
                cancel_source: None,
                scope: ReadinessScope::Endpoint,
                media: None,
                operation: [8; 16],
                endpoint,
                tape: None,
                barcode: None,
                phase: ReadinessPhase::Ready,
                state: ReadinessState::Ready,
                previous: None,
                started_unix_ns: 0,
                limit_ms: Some(1),
                evidence: None,
                load_intent: None,
                ready_generation: 1,
                result: ReadinessResult::Ready,
            }),
            &Present,
        )
        .unwrap();
        assert_eq!(fold.loads.len(), 0);
        assert_eq!(fold.load_endpoints.len(), 0);
        assert_eq!(fold.retained_records(), 0);
        assert_eq!(fold.idempotency_bytes(), 0);
    }
}

#[cfg(test)]
mod contradiction_tests {
    use super::*;
    use crate::library_log::tests::*;

    /// D6: injected liveness loss is reported on the fence, never the tape summary.
    /// A healthy production history cannot lose a provisioned identity (C-P4/D6).
    #[test]
    fn b4b0_d6_unknown_mismatched_retired_deferred_fence() {
        for cause in [
            FoldError::UnknownTape,
            FoldError::IncarnationMismatch,
            FoldError::RetiredIdentity,
        ] {
            let mut f = LibraryFold::default();
            let mut h = History::default();
            b4b0_receive(&mut f, &mut h, provision(1, None), &Present).unwrap();
            b4b0_receive(&mut f, &mut h, verdict(), &View::default()).unwrap();
            b4b0_receive(&mut f, &mut h, fence(1), &View::default()).unwrap();
            // Inject a lost predecessor/identity, preserving pending-byte accounting.
            let predecessor = f.pending.remove(0);
            f.pending_bytes -= predecessor.bytes.len();
            match cause {
                FoldError::UnknownTape => {
                    f.tapes.remove(&tape(1).uuid);
                }
                FoldError::IncarnationMismatch => {
                    f.tapes.get_mut(&tape(1).uuid).unwrap().tape.incarnation = 2
                }
                FoldError::RetiredIdentity => {
                    f.tapes.get_mut(&tape(1).uuid).unwrap().retired = true
                }
                _ => unreachable!(),
            }
            f.resolve(&Present, &h).unwrap();
            assert_eq!(f.contradicted_tapes(), 0);
            assert_eq!(f.open_fences(), 1);
            assert!(f.pending.is_empty());
            assert_eq!(f.pending_bytes(), 0);
            assert_eq!(f.fence_liveness_failures.get(&3), Some(&(tape(1), cause)));
            b4b0_receive(&mut f, &mut h, release(3), &Present).unwrap();
            assert!(f.fence_liveness_failures.is_empty());
            assert_eq!(f.open_fences(), 0);
        }
    }
}

#[cfg(test)]
mod verdict_shape_tests {
    use super::*;
    use crate::library_log::tests::*;

    /// Unreferenced verdicts never query native evidence, including behind scope blockers.
    struct NoNative;
    impl TapeView for NoNative {
        fn lookup(&self, _: &TapeAt) -> Result<TapeLookup, FoldError> {
            panic!("unreferenced verdict consulted native evidence")
        }
    }

    /// D-W-7 preserves tape/operation order, excludes labels and records no native floor.
    #[test]
    fn unreferenced_verdict_orders_without_native_evidence() {
        let mut f = LibraryFold::default();
        let mut h = History::default();
        b4b0_receive(&mut f, &mut h, provision(1, Some("A")), &Present).unwrap();
        b4b0_receive(&mut f, &mut h, provision(2, Some("B")), &Present).unwrap();
        b4b0_receive(&mut f, &mut h, provision(3, Some("C")), &Present).unwrap();
        b4b0_receive(&mut f, &mut h, verdict(), &View::default()).unwrap();
        for (t, op, held) in [(1, 8, true), (2, 7, true), (3, 9, false)] {
            let Body::VerificationVerdict(mut v) = verdict() else {
                unreachable!()
            };
            v.at = None;
            v.tape = tape(t);
            v.operation = [op; 16];
            let b = Body::VerificationVerdict(v);
            assert_eq!(
                f.scopes_for(&b, &h).unwrap(),
                BTreeSet::from([Scope::Tape(tape(t).uuid), Scope::Operation([op; 16])])
            );
            let pending = f.pending.len();
            b4b0_receive(&mut f, &mut h, b, &NoNative).unwrap();
            assert_eq!(f.pending.len(), pending + usize::from(held));
            assert_eq!(f.highest_reference(tape(2)), None);
        }
        // Replay resolution can gain native evidence without invalidating None verdicts.
        f.resolve(&Present, &h).unwrap();
        assert!(f.pending.is_empty());
        assert_eq!(f.highest_reference(tape(1)), Some(1));
        assert_eq!(f.highest_reference(tape(2)), None);
    }

    /// Verdict identity, outcome and both verification mode masks are shape constraints.
    #[test]
    fn verification_shape_precedes_native_lookup() {
        let f = LibraryFold::default();
        for modes in [0, 128, u32::MAX] {
            for mut b in [verdict(), progress(1)] {
                match &mut b {
                    Body::VerificationVerdict(v) => v.modes = modes,
                    Body::VerificationProgress(v) => v.modes = modes,
                    _ => unreachable!(),
                }
                assert_eq!(
                    f.validate_shape(1, &record(b, 1)),
                    Err(FoldError::InvalidSchema)
                );
            }
        }
        let Body::VerificationVerdict(mut v) = verdict() else {
            unreachable!()
        };
        v.tape = tape(2);
        assert_eq!(
            f.validate_shape(1, &record(Body::VerificationVerdict(v.clone()), 1)),
            Err(FoldError::InvalidSchema)
        );
        v.tape = tape(1);
        v.damaged = Bounded(vec![DamagedRange {
            partition: 0,
            start_lba: 1,
            end_lba: 2,
        }]);
        assert_eq!(
            f.validate_shape(1, &record(Body::VerificationVerdict(v), 1)),
            Err(FoldError::InvalidSchema)
        );
    }
}
