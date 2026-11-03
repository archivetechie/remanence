//! Pure causal validation and resource-scoped replay. Presentation text never selects a rule.
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
    /// Absent or below the requested closed sequence.
    NotYet,
    /// Identity or content contradiction.
    Contradiction,
}
/// Read-only native proof lookup supplied by the authority owner.
pub trait TapeView {
    /// Look up immutable native evidence.
    fn lookup(&self, at: &TapeAt) -> TapeLookup;
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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    /// A rule assignment must be recomputed under the current configuration.
    #[error("StaleConfiguration")]
    StaleConfiguration,
    /// Historical storage could not be read; never treat I/O failure as absence.
    #[error("HistoryUnavailable")]
    HistoryUnavailable,
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
    /// Live shell label.
    pub label: Option<LabelText>,
    /// Current pool.
    pub pool: Option<Text>,
    /// Current calibration generation.
    pub calibration_generation: u64,
}
#[derive(Clone, Debug)]
struct Held {
    sequence: u64,
    record: Record,
    receipt: Receipt,
}
/// An unresolved record with stable causal identity and affected scopes.
#[derive(Clone, Debug)]
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
}
/// Result of validation, reused by append and replay.
#[derive(Clone, Debug)]
pub enum Validated {
    /// The candidate can change state.
    Apply,
    /// Existing identical authority; no bytes should be appended.
    Duplicate(Receipt),
}
/// O(resources), plus pending up to the cap, plus one looked-up frame.
/// Pending bodies are canonical bytes, not a retained decoded record history.
#[derive(Clone, Debug, Default)]
pub struct LibraryFold {
    /// Last received frame.
    pub received_through: u64,
    /// Highest contiguous successfully applied prefix, never crossing pending.
    pub applied_through: u64,
    /// Number of identical duplicate records encountered on replay.
    pub duplicates: u64,
    /// Current tape identities.
    pub tapes: BTreeMap<Id, TapeState>,
    /// Pending records in source sequence order.
    pub pending: Vec<PendingRecord>,
    /// Cross-authority contradictions, isolated to affected resources.
    pub contradictions: BTreeMap<Scope, FoldError>,
    /// Missing fence evidence, which never delays the fence.
    pub unresolved_fence_evidence: BTreeSet<u64>,
    pending_bytes: usize,
    latest: BTreeMap<(Tag, Scope, Option<Id>), Held>,
    fences: BTreeMap<u64, Held>,
    intents: BTreeMap<u64, Held>,
    uncertain: BTreeSet<u64>,
    loads: BTreeMap<(Id, DriveKey), u32>,
    inventories: BTreeMap<Text, u64>,
    drives: BTreeMap<Id, (u64, bool)>,
    readiness: BTreeMap<DriveKey, u64>,
    idempotency: BTreeMap<(Text, OperationKind, Id), (Id, Hash)>,
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
            Self::SupersedeTail(v) => Some(v.operation),
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
    fn at(&self) -> Option<&TapeAt> {
        match self {
            Self::SupersedeTail(v) => Some(&v.at),
            Self::AcceptUnprotectedFinalization(v) => Some(&v.at),
            Self::VerificationProgress(v) => Some(&v.at),
            Self::VerificationVerdict(v) => Some(&v.at),
            Self::ManualFinalizationAccepted(v) => Some(&v.at),
            Self::RecoveryRecorded(v) => Some(&v.at),
            _ => None,
        }
    }
    fn tapes(&self) -> Vec<TapeRef> {
        match self {
            Self::AssignPool(v) => vec![v.tape],
            Self::Label(v) => std::iter::once(v.tape)
                .chain(v.prior_holder.as_ref().map(|x| x.tape))
                .collect(),
            Self::Retire(v) => vec![v.tape],
            Self::MediaGenerationSet(v) => vec![v.tape],
            Self::WrapMapCached(v) => vec![v.tape],
            Self::FenceRaised(v) => {
                let mut t = match &v.scope {
                    FenceScope::Tape(t) => vec![*t],
                    _ => vec![],
                };
                if let Some(at) = &v.at {
                    t.push(at.tape);
                }
                t
            }
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
            Self::MotionOutcome(v) => vec![v.intent],
            Self::InventoryReconciled(v) => v.resolves.0.clone(),
            Self::VerificationVerdict(v) => v.progress.into_iter().collect(),
            Self::ReadinessObserved(v) => v.previous.into_iter().chain(v.load_intent).collect(),
            Self::CleaningRecorded(v) => v.previous.into_iter().collect(),
            Self::AlarmChanged(v) => v.cause.into_iter().collect(),
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
                s.insert(Scope::Library(v.library_serial.clone()));
            }
            Self::ConfigurationAccepted(_) => {
                s.insert(Scope::Config);
            }
            Self::ReadinessObserved(v) => {
                s.insert(Scope::Endpoint(v.endpoint.clone()));
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
            Self::MediaGenerationSet(v) => Some(Scope::Tape(v.tape.uuid)),
            Self::ManualFinalizationAccepted(v) => Some(Scope::Tape(v.at.tape.uuid)),
            Self::WrapMapCached(v) => Some(Scope::Tape(v.tape.uuid)),
            Self::DriveObserved(v) => Some(Scope::Drive(v.drive_uuid)),
            Self::DriveDecision(v) => Some(Scope::Drive(v.drive_uuid)),
            Self::DriveHealthObserved(v) => Some(Scope::Drive(v.drive_uuid)),
            Self::ConfigurationAccepted(_) => Some(Scope::Config),
            Self::AlarmChanged(v) => Some(Scope::Alarm(v.condition_key.clone())),
            Self::CleaningRecorded(v) => {
                if matches!(
                    v.action,
                    CleaningAction::Register(_) | CleaningAction::Expire(_)
                ) {
                    v.cartridge.map(|tape| Scope::Tape(tape.uuid))
                } else {
                    Some(Scope::CleaningRun(v.run_id.clone()))
                }
            }
            Self::SessionObserved(v) => Some(Scope::Operation(v.session_id)),
            Self::SupersedeTail(v) => Some(Scope::Operation(v.session_id)),
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
        let v = self.tapes.get(&t.uuid).ok_or(FoldError::UnknownTape)?;
        if v.tape != t {
            return Err(FoldError::IncarnationMismatch);
        }
        if v.retired {
            return Err(FoldError::RetiredIdentity);
        }
        Ok(v)
    }
    fn find(&self, r: DecisionRef) -> Option<&Held> {
        self.latest
            .values()
            .chain(self.fences.values())
            .chain(self.intents.values())
            .find(|v| v.sequence == r.sequence && v.record.envelope.record_uuid == r.record_uuid)
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
    fn check_tape(&self, b: &Body, view: &dyn TapeView) -> Result<(), FoldError> {
        if let Some(at) = b.at() {
            match view.lookup(at) {
                TapeLookup::NotYet => return Err(FoldError::Unresolved),
                TapeLookup::Contradiction => return Err(FoldError::TapeContradiction),
                TapeLookup::Present { kind, digest } => {
                    if matches!(b, Body::ManualFinalizationAccepted(_))
                        && kind != crate::tape_log::Tag::FinalizationPrepared
                    {
                        return Err(FoldError::CounterpartKind);
                    }
                    if digest != at.record_digest {
                        return Err(FoldError::TapeContradiction);
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
        let history = &HistoryPrefix {
            history,
            before: self.received_through + 1,
        };
        if !matches!(r.body, Body::FenceRaised(_)) {
            let scopes = self.scopes_for(&r.body, history)?;
            if self
                .contradictions
                .keys()
                .any(|scope| scopes.contains(scope))
            {
                return Err(FoldError::TapeContradiction);
            }
            if self.pending.iter().any(|p| !p.scope.is_disjoint(&scopes)) {
                self.check_pending_capacity(r, &scopes)?;
                return Err(FoldError::Unresolved);
            }
        }
        self.validate_at(self.received_through + 1, r, view, history)
    }
    fn validate_shape(&self, sequence: u64, r: &Record) -> Result<(), FoldError> {
        let b = &r.body;
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
        let at = b.at().or(match b {
            Body::FenceRaised(v) => v.at.as_ref(),
            _ => None,
        });
        if at.is_some_and(|at| at.sequence == 0) {
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
        if let Body::AssignPool(v) = b {
            if v.source == AssignmentSource::Rule {
                let reference = v.configuration.ok_or(FoldError::WrongCounterpart)?;
                let latest = self
                    .last(Tag::ConfigurationAccepted, Scope::Config)
                    .ok_or(FoldError::WrongCounterpart)?;
                if latest.record.reference(latest.sequence) != reference {
                    return Err(FoldError::StaleConfiguration);
                }
            }
        }
        // Retirement dominates duplicate handling: an old decision cannot resurrect an identity.
        for t in b.tapes() {
            if t.incarnation == 0 {
                return Err(FoldError::InvalidSchema);
            }
            self.tape(t)?;
        }
        if let Some(at) = b.at() {
            if at.sequence == 0 {
                return Err(FoldError::InvalidSchema);
            }
        }
        if let Body::FenceReleased(v) = b {
            if let Some(old) = self
                .latest
                .values()
                .find(|h| matches!(&h.record.body,Body::FenceReleased(r) if r.fence==v.fence))
            {
                return if old.record.body == *b {
                    Ok(Validated::Duplicate(old.receipt.clone()))
                } else {
                    Err(FoldError::FenceAlreadyReleased)
                };
            }
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
                    || v.block_size == 0
                    || v.pool.as_ref().is_some_and(Text::is_empty)
                {
                    return Err(FoldError::InvalidSchema);
                }
                if v.label.as_ref().is_some_and(|l| {
                    l.is_empty()
                        || self
                            .tapes
                            .values()
                            .any(|t| !t.retired && t.label.as_ref() == Some(l))
                }) {
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
                        || !v.block_size.is_multiple_of(512)
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
                    self.tapes
                        .values()
                        .find(|t| !t.retired && t.label.as_ref() == Some(l) && t.tape != v.tape)
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
            MediaGenerationSet(v) => generation(&v.change, self.tape(v.tape)?.media_revision)?,
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
                if !self.fences.contains_key(&v.fence) {
                    let retained = self
                        .latest
                        .values()
                        .chain(self.intents.values())
                        .find(|h| h.sequence == v.fence);
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
                if self
                    .intents
                    .values()
                    .any(|h| matches!(&h.record.body,Body::MotionIntent(i) if i.command==v.command))
                {
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
                        (Motion::Load(_), Some(Confirmation::Readiness(_)))
                            | (
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
                    <= self
                        .inventories
                        .get(&v.library_serial)
                        .copied()
                        .unwrap_or(0)
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
                    if !motion_scopes(&i.motion).contains(&Scope::Library(v.library_serial.clone()))
                    {
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
                    if p.at != v.at || p.modes != v.modes {
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
                    CleaningAction::Register(_) | CleaningAction::Expire(_)
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
                        && self.latest.values().any(|h| matches!(&h.record.body,
                            Body::CleaningRecorded(c) if c.run_id != v.run_id
                                && !matches!(c.action, CleaningAction::Finish(_) | CleaningAction::Expire(_) | CleaningAction::Register(_))
                                && (v.drive_uuid.is_some() && v.drive_uuid == c.drive_uuid
                                    || v.cartridge.is_some() && v.cartridge == c.cartridge))) {
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
                        if !raised
                            || old.is_some_and(|a| {
                                matches!(a.action, AlarmAction::Acknowledge(_))
                            }) =>
                    {
                        return Err(FoldError::AlarmState)
                    }
                    AlarmAction::Acknowledge(_) if r.envelope.actor == AuditActor::System => {
                        return Err(FoldError::SystemAcknowledgement)
                    }
                    AlarmAction::Clear(_) if !raised => return Err(FoldError::AlarmState),
                    _ => {}
                }
                if let Some(d) = v.cause {
                    self.historical(d, history)?
                        .ok_or(FoldError::WrongCounterpart)?;
                }
            }
            OperationRecorded(v) => {
                let old = self.chain(
                    Tag::OperationRecorded,
                    Scope::Operation(v.operation),
                    v.previous,
                )?;
                if old.is_some_and(|h|matches!(&h.record.body,Body::OperationRecorded(o) if terminal(&o.event)||o.kind!=v.kind)){return Err(FoldError::OperationState);}
                match &v.event{
                    OperationEvent::Accepted(a)=>{if old.is_some(){return Err(FoldError::OperationState);}
if let Some(k)=a.idempotency_key{if self.idempotency.get(&(a.actor_fingerprint.clone(),v.kind,k)).is_some_and(|x|*x!=(v.operation,a.request_fingerprint)){return Err(FoldError::ConflictingDuplicate);}}},
                    OperationEvent::Started(_)=>{if old.is_some_and(|h|!matches!(&h.record.body,Body::OperationRecorded(o) if matches!(o.event,OperationEvent::Accepted(_)))){return Err(FoldError::OperationState);}},
                    _=>{if old.is_none(){return Err(FoldError::OperationState);}}
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
                if matches!(&v.action,RecoveryAction::RepairApplied(p) if p.retained_through!=v.at.sequence)
                {
                    return Err(FoldError::InvalidSchema);
                }
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
                if v.detail.0.windows(2).any(|w| w[0].0 >= w[1].0) {
                    return Err(FoldError::InvalidSchema);
                }
            }
            SupersedeTail(_) => {}
        }
        self.check_tape(b, view)?;
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
        match &result {
            Validated::Apply => self.commit(sequence, record, receipt, view),
            Validated::Duplicate(_) => self.duplicates += 1,
        }
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
    ) {
        self.commit(sequence, record, receipt, view);
        self.received_through = sequence;
        self.watermark();
    }
    fn commit(&mut self, sequence: u64, record: Record, receipt: Receipt, view: &dyn TapeView) {
        let b = &record.body;
        use Body::*;
        match b {
            Reprovision(v) => {
                self.tapes.insert(
                    v.tape.uuid,
                    TapeState {
                        tape: v.tape,
                        retired: false,
                        assignment_generation: 1,
                        label_generation: 1,
                        media_revision: 1,
                        label: v.label.clone(),
                        pool: v.pool.clone(),
                        calibration_generation: 0,
                    },
                );
            }
            AssignPool(v) => {
                let t = self.tapes.get_mut(&v.tape.uuid).unwrap();
                t.assignment_generation = v.change.next;
                t.pool = v.pool.clone();
            }
            Label(v) => {
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
                let t = self.tapes.get_mut(&v.tape.uuid).unwrap();
                t.retired = true;
                t.pool = None;
                t.label = None;
            }
            MediaGenerationSet(v) => {
                self.tapes.get_mut(&v.tape.uuid).unwrap().media_revision = v.change.next
            }
            FenceRaised(v) => {
                if let Some(at) = &v.at {
                    if !matches!(view.lookup(at),TapeLookup::Present{digest,..} if digest==at.record_digest)
                    {
                        self.unresolved_fence_evidence.insert(sequence);
                    }
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
                let raised = self.fences.remove(&v.fence).unwrap();
                self.unresolved_fence_evidence.remove(&v.fence);
                let Body::FenceRaised(raised) = &raised.record.body else {
                    unreachable!()
                };
                let scope = match &raised.scope {
                    FenceScope::Tape(tape) => Scope::Tape(tape.uuid),
                    FenceScope::Drive(endpoint) => Scope::Endpoint(endpoint.clone()),
                };
                self.latest.insert(
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
                    self.intents.remove(&v.intent.sequence);
                }
            }
            InventoryReconciled(v) => {
                self.inventories
                    .insert(v.library_serial.clone(), v.observation_generation);
                for d in &v.resolves.0 {
                    self.intents.remove(&d.sequence);
                    self.uncertain.remove(&d.sequence);
                }
            }
            ReadinessObserved(v) => {
                self.readiness
                    .insert(v.endpoint.clone(), v.ready_generation);
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
                            (a.actor_fingerprint.clone(), v.kind, k),
                            (v.operation, a.request_fingerprint),
                        );
                    }
                }
            }
            WrapMapCached(v) => {
                self.tapes
                    .get_mut(&v.tape.uuid)
                    .unwrap()
                    .calibration_generation = v.calibration_generation
            }
            _ => {}
        }
        if let Some(slot) = b.slot() {
            let key = (b.tag(), slot, b.slot_tape());
            if matches!(b,SessionObserved(v) if v.event!=SessionEvent::Opened) {
                self.latest.remove(&key);
            } else {
                self.latest.insert(
                    key,
                    Held {
                        sequence,
                        record,
                        receipt,
                    },
                );
            }
        }
    }
    fn scopes_for(
        &self,
        b: &Body,
        history: &dyn LibraryHistory,
    ) -> Result<BTreeSet<Scope>, FoldError> {
        let mut s = b.scopes();
        for t in b.tapes() {
            if let Some(l) = self.tapes.get(&t.uuid).and_then(|t| t.label.as_ref()) {
                s.insert(Scope::Label(l.clone()));
            }
        }
        for d in b.refs() {
            // Rule configurations use only the retained latest configuration.
            if matches!(b, Body::AssignPool(v) if v.source == AssignmentSource::Rule) {
                s.insert(Scope::Config);
            } else if let Some(h) = self.historical(d, history)? {
                s.extend(h.body.scopes());
            }
        }
        if let Body::FenceReleased(v) = b {
            if let Some(h) = self.fences.get(&v.fence) {
                s.extend(h.record.body.scopes());
            }
        }
        Ok(s)
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
        if sequence != self.received_through + 1 {
            return Err(FoldError::Sequence);
        }
        self.validate_shape(sequence, &record)?;
        let history = &HistoryPrefix {
            history,
            before: sequence,
        };
        let scope = self.scopes_for(&record.body, history)?;
        let blocked = self.pending.iter().any(|p| !p.scope.is_disjoint(&scope))
            || self.contradictions.keys().any(|s| scope.contains(s));
        // Unblocked records must first be self-consistent library history. For a queued
        // scope only shape and native counterpart checks are decidable until predecessors apply.
        let native = if blocked {
            self.check_tape(&record.body, view)
        } else {
            self.validate_at(sequence, &record, view, history)
                .map(|_| ())
        };
        match native {
            Err(FoldError::TapeContradiction) => {
                self.contradictions.insert(
                    Scope::Tape(record.body.at().unwrap().tape.uuid),
                    FoldError::TapeContradiction,
                );
                self.received_through = sequence;
                self.watermark();
                return Ok(());
            }
            Err(FoldError::Unresolved) | Ok(()) => {}
            Err(e) => return Err(e),
        }
        if blocked && !matches!(record.body, Body::FenceRaised(_)) {
            self.hold(sequence, record, receipt, scope)?;
        } else {
            match self.validate_at(sequence, &record, view, history) {
                Ok(Validated::Apply) => self.commit(sequence, record, receipt, view),
                Ok(Validated::Duplicate(_)) => self.duplicates += 1,
                Err(FoldError::Unresolved) => self.hold(sequence, record, receipt, scope)?,
                Err(FoldError::TapeContradiction) => {
                    let at = record.body.at().unwrap();
                    self.contradictions
                        .insert(Scope::Tape(at.tape.uuid), FoldError::TapeContradiction);
                }
                Err(e) => return Err(e),
            }
        }
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
        loop {
            let before = self.pending.len();
            let pending = std::mem::take(&mut self.pending);
            let mut it = pending.into_iter();
            while let Some(p) = it.next() {
                if self.pending.iter().any(|q| !q.scope.is_disjoint(&p.scope))
                    || self.contradictions.keys().any(|s| p.scope.contains(s))
                {
                    self.pending.push(p);
                    continue;
                }
                let record = match wire::decode(p.tag as u16, &p.bytes) {
                    Ok(record) => record,
                    Err(_) => {
                        self.pending.push(p);
                        self.pending.extend(it);
                        self.watermark();
                        return Err(FoldError::InvalidSchema);
                    }
                };
                match self.validate_at(p.sequence, &record, view, history) {
                    Ok(Validated::Apply) => {
                        self.pending_bytes -= p.bytes.len();
                        self.commit(p.sequence, record, p.receipt, view);
                    }
                    Ok(Validated::Duplicate(_)) => {
                        self.pending_bytes -= p.bytes.len();
                        self.duplicates += 1;
                    }
                    Err(FoldError::Unresolved) => self.pending.push(p),
                    Err(FoldError::TapeContradiction) => {
                        self.pending_bytes -= p.bytes.len();
                        self.contradictions.insert(
                            Scope::Tape(record.body.at().unwrap().tape.uuid),
                            FoldError::TapeContradiction,
                        );
                    }
                    Err(e) => {
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
    /// Open fences, individually identified by their raise sequence.
    pub fn open_fences(&self) -> usize {
        self.fences.len()
    }
    /// Whether a changer or standalone endpoint has unresolved motion.
    pub fn inventory_unknown(&self, scope: &Scope) -> bool {
        self.intents
            .values()
            .any(|h| h.record.body.scopes().contains(scope))
    }
    /// Native tape fold's typed crash acknowledgment lookup.
    pub fn supersede(&self, sequence: u64) -> Option<&SupersedeTail> {
        self.latest.values().find_map(|h| {
            if h.sequence == sequence {
                if let Body::SupersedeTail(v) = &h.record.body {
                    Some(v)
                } else {
                    None
                }
            } else {
                None
            }
        })
    }
}
fn duplicate_key(a: &Body, b: &Body) -> bool {
    match (a, b) {
        (Body::AssignPool(a), Body::AssignPool(b)) => a.change.expected == b.change.expected,
        (Body::Label(a), Body::Label(b)) => a.change.expected == b.change.expected,
        (Body::MediaGenerationSet(a), Body::MediaGenerationSet(b)) => {
            a.change.expected == b.change.expected
        }
        (Body::AlarmChanged(a), Body::AlarmChanged(b)) => a.change.expected == b.change.expected,
        (Body::DriveDecision(a), Body::DriveDecision(b)) => a.change.expected == b.change.expected,
        (Body::OperationRecorded(a), Body::OperationRecorded(b)) => a.previous == b.previous,
        (Body::ReadinessObserved(a), Body::ReadinessObserved(b)) => a.previous == b.previous,
        (Body::CleaningRecorded(a), Body::CleaningRecorded(b)) => {
            !matches!(
                b.action,
                CleaningAction::Register(_) | CleaningAction::Expire(_)
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
