//! Transactional streaming fold. Only closed groups replace TapeSummary.
//! Pending and committed summaries each hold at most E uncovered extents; rows,
//! catalog members, and directory history are never collected. Work is linear
//! in new bytes and rows; appending never scans the preceding log.

use super::{
    codec::{CodecError, Decoder, Frame, Tail, TornKind},
    payload::CatalogValidator,
    *,
};
use remanence_parity::{TapeFileKind, TerminalTailComponentKind};
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, io::Read};

/// Four-word intersection of a physical Object with the uncovered suffix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UncoveredExtent {
    /// First retained data ordinal.
    pub first: u64,
    /// End-exclusive retained ordinal.
    pub end: u64,
    /// Physical file number.
    pub tape_file_number: u64,
    /// Physical LBA corresponding to first.
    pub start_lba: u64,
}
/// Constant-size finalization state; diagnostics/catalog/directory stay in the log.
#[derive(Clone, Debug)]
pub struct FinalizationState {
    /// Sequence of the combined plan or compatibility preparation.
    pub sequence: u64,
    /// Immutable plan stream location and digest.
    pub reference: GroupRef,
    /// Edition digest.
    pub edition_digest: [u8; 32],
    /// Exact five-file layout.
    pub layout: TerminalTailLayout,
    /// Trigger retained for diagnostics.
    pub trigger: TerminalFinalizationTrigger,
    /// Provisional manual preparation, removed at step 6.
    pub prepared: bool,
    /// Existing compatibility flag, removed at step 6.
    pub recovery_required: bool,
    /// Number of barrier-proved tail components.
    pub components_written: u8,
    /// Whether the planned prefix has been proved.
    pub prefix_written: bool,
    plan_identity: [u8; 32],
    prefix_digest: [u8; 32],
    prefix_rows: u64,
}
/// Bounded authoritative state; a fresh tape is represented by Default.
#[derive(Clone, Debug)]
pub struct TapeSummary {
    /// None until the first group closes.
    pub tape_uuid: Option<[u8; 16]>,
    /// Zero when fresh, one once initialized.
    pub incarnation: u64,
    /// Host format version.
    pub version: u16,
    /// Last authoritative closing sequence, including metadata events.
    pub last_closed_sequence: u64,
    /// Exclusive byte end of authoritative prefix.
    pub closed_end: u64,
    /// Fixed block size, zero when fresh.
    pub block_size: u32,
    /// Bound parity geometry.
    pub scheme: Option<ParityScheme>,
    /// Last media append boundary; metadata cannot replace this.
    pub cursor: CommitCursor,
    /// W.
    pub protected: u64,
    /// T.
    pub total: u64,
    /// Check over [W,T).
    pub check: CheckValue,
    /// Check origin, explicitly unverified after cold rebuild.
    pub provenance: CheckProvenance,
    /// End of the rebuilt prefix that still lacks comparison with prior authority.
    pub unverified_end: Option<u64>,
    /// Next actual sidecar ID.
    pub next_epoch: u64,
    /// Next physical file number.
    pub next_file: u64,
    /// Cumulative Object count.
    pub object_count: u64,
    /// Last ordinary Committed sequence.
    pub last_ordinary_commit: Option<u64>,
    /// Constant-size last ordinary closer; never retains catalog payloads.
    pub last_ordinary_record: Option<Committed>,
    /// Last barrier record sequence.
    pub last_media_commit: Option<u64>,
    /// API barrier ordinal, separate from sequence and journal_record_ordinal.
    pub barrier_count: u64,
    /// Ordinary checkpoint barriers, excluding terminal media barriers.
    pub ordinary_count: u64,
    /// Outstanding seal obligation.
    pub seal_due: Option<SealDueReason>,
    /// Durable replica C followed by Sealed.
    pub sealed: bool,
    /// At most E positive-size intersections with [W,T).
    pub uncovered: VecDeque<UncoveredExtent>,
    /// Exact constant-size terminal plan/progress reference.
    pub finalization: Option<FinalizationState>,
    sidecar_identity: Sha256,
}
impl Default for TapeSummary {
    fn default() -> Self {
        Self {
            tape_uuid: None,
            incarnation: 0,
            version: VERSION,
            last_closed_sequence: 0,
            closed_end: 0,
            block_size: 0,
            scheme: None,
            cursor: CommitCursor::default(),
            protected: 0,
            total: 0,
            check: CheckValue::EMPTY,
            provenance: CheckProvenance::Empty,
            unverified_end: None,
            next_epoch: 0,
            next_file: 0,
            object_count: 0,
            last_ordinary_commit: None,
            last_ordinary_record: None,
            last_media_commit: None,
            barrier_count: 0,
            ordinary_count: 0,
            seal_due: None,
            sealed: false,
            uncovered: VecDeque::new(),
            finalization: None,
            sidecar_identity: Sha256::new(),
        }
    }
}
impl TapeSummary {
    // Clone constant metadata only. Pending extents contain new Objects, while
    // the authoritative deque remains untouched until successful group closure.
    fn pending_copy(&self) -> Self {
        Self {
            tape_uuid: self.tape_uuid,
            incarnation: self.incarnation,
            version: self.version,
            last_closed_sequence: self.last_closed_sequence,
            closed_end: self.closed_end,
            block_size: self.block_size,
            scheme: self.scheme.clone(),
            cursor: self.cursor,
            protected: self.protected,
            total: self.total,
            check: self.check,
            provenance: self.provenance,
            unverified_end: self.unverified_end,
            next_epoch: self.next_epoch,
            next_file: self.next_file,
            object_count: self.object_count,
            last_ordinary_commit: self.last_ordinary_commit,
            last_ordinary_record: self.last_ordinary_record.clone(),
            last_media_commit: self.last_media_commit,
            barrier_count: self.barrier_count,
            ordinary_count: self.ordinary_count,
            seal_due: self.seal_due,
            sealed: self.sealed,
            uncovered: VecDeque::new(),
            finalization: self.finalization.clone(),
            sidecar_identity: self.sidecar_identity.clone(),
        }
    }
    /// Maximum uncovered blocks for this geometry; zero for parity-off.
    pub fn epoch_blocks(&self) -> u64 {
        self.scheme.as_ref().map_or(0, |s| {
            u64::from(s.data_blocks_per_stripe) * u64::from(s.stripes_per_neighborhood)
        })
    }
    /// Fresh means there is no closed initialization, even if a pending one was seen.
    pub fn is_fresh(&self) -> bool {
        self.tape_uuid.is_none()
    }
}

/// Incremental group reference builder shared by encoders and the fold.
#[derive(Clone, Debug)]
pub struct GroupDigest {
    reference: GroupRef,
    digest: Sha256,
}
impl GroupDigest {
    /// Start at the first provisional (or closing, for an empty group) frame.
    pub fn new(first_sequence: u64, start_offset: u64) -> Self {
        Self {
            reference: GroupRef {
                first_sequence,
                start_offset,
                ..GroupRef::default()
            },
            digest: Sha256::new(),
        }
    }
    /// Bind one validated provisional frame. Never call this with the closing record.
    pub fn push(&mut self, frame: &Frame<'_>) -> Result<(), Fault> {
        if !frame.payload_validated {
            return Err(Fault::Invalid("group digest needs payload validation"));
        }
        let expected = add(self.reference.first_sequence, self.reference.frame_count)?;
        if frame.sequence != expected {
            return Err(Fault::Sequence {
                expected,
                actual: frame.sequence,
            });
        }
        self.reference.frame_count = add(self.reference.frame_count, 1)?;
        self.reference.byte_count = add(
            self.reference.byte_count,
            (frame.structural.len() + frame.payload.len()) as u64,
        )?;
        self.digest.update(frame.prefix);
        self.digest.update(frame.structural);
        self.digest.update(frame.payload);
        Ok(())
    }
    /// Finish with the independently decoded member count.
    pub fn reference(&self, member_count: u64) -> GroupRef {
        GroupRef {
            member_count,
            digest: self.digest.clone().finalize().into(),
            ..self.reference.clone()
        }
    }
}
#[derive(Debug)]
struct Catalog {
    file: u64,
    ordinal: u64,
    validator: CatalogValidator,
}
#[derive(Debug)]
struct Pending {
    state: TapeSummary,
    group: GroupDigest,
    catalog: Option<Catalog>,
    members: u64,
    rows: u64,
    objects: u64,
    maps: u64,
    components: u64,
    row_digest: Sha256,
    directory: Sha256,
    directory_count: u64,
    directory_file: Option<u64>,
}
impl Pending {
    fn new(summary: &TapeSummary, sequence: u64, offset: u64) -> Self {
        Self {
            state: summary.pending_copy(),
            group: GroupDigest::new(sequence, offset),
            catalog: None,
            members: 0,
            rows: 0,
            objects: 0,
            maps: 0,
            components: 0,
            row_digest: Sha256::new(),
            directory: Sha256::new(),
            directory_count: 0,
            directory_file: None,
        }
    }
    fn reference(&self) -> GroupRef {
        self.group.reference(self.members)
    }
}
/// Stateful fold over a single complete frame stream. After an error it is poisoned.
#[derive(Debug)]
pub struct Fold {
    summary: TapeSummary,
    pending: Option<Pending>,
    next_sequence: u64,
    poisoned: bool,
}
impl Default for Fold {
    fn default() -> Self {
        Self {
            summary: TapeSummary::default(),
            pending: None,
            next_sequence: 1,
            poisoned: false,
        }
    }
}
impl Fold {
    /// Resume only a summary obtained by a full validated immutable-prefix scan.
    pub(super) fn from_validated(summary: TapeSummary) -> Self {
        Self {
            next_sequence: summary.last_closed_sequence + 1,
            summary,
            pending: None,
            poisoned: false,
        }
    }
    /// Begin at a fresh tape. Streaming snapshots/resume are a later storage concern.
    pub fn new() -> Self {
        Self {
            next_sequence: 1,
            ..Self::default()
        }
    }
    /// Last closed authority only; pending changes are never visible here.
    pub fn summary(&self) -> &TapeSummary {
        &self.summary
    }
    /// Validate exactly one complete candidate group without changing or poisoning
    /// this fold. B3a calls this before append. Requires a closed current boundary.
    /// The trial omits old extents: their already-validated contiguous range is
    /// described by W/T, and no trial summary is published. Thus preflight does
    /// not copy O(E) old extents or collect the candidate's bytes in memory.
    pub fn preflight(&self, reader: impl Read) -> Result<(), CodecError> {
        let offset = self.summary.closed_end;
        let invalid = |why| TapeLogError {
            offset,
            sequence: Some(self.next_sequence),
            fault: Fault::Invalid(why),
        };
        if self.poisoned {
            return Err(invalid("fold poisoned").into());
        }
        if self.pending.is_some() {
            return Err(invalid("preflight requires closed boundary").into());
        }
        let trial = self.preflight_prefix(reader, true)?;
        if trial.summary.last_closed_sequence < self.next_sequence || trial.pending.is_some() {
            return Err(invalid("candidate must contain one closed group").into());
        }
        Ok(())
    }
    /// Retain an unpublished trial across the media barrier. With `closed` false,
    /// only provisional frames are accepted; the caller validates the closer later.
    pub(super) fn preflight_prefix(
        &self,
        reader: impl Read,
        closed: bool,
    ) -> Result<Self, CodecError> {
        let invalid = |why| {
            CodecError::Recovery(TapeLogError {
                offset: self.summary.closed_end,
                sequence: Some(self.next_sequence),
                fault: Fault::Invalid(why),
            })
        };
        if self.poisoned || self.pending.is_some() {
            return Err(invalid("preflight requires healthy closed boundary"));
        }
        let mut trial = Self {
            summary: self.summary.pending_copy(),
            pending: None,
            next_sequence: self.next_sequence,
            poisoned: false,
        };
        let mut decoder = Decoder::at(reader, self.next_sequence, self.summary.closed_end);
        while let Some(frame) = decoder.next_frame()? {
            if trial.summary.last_closed_sequence >= self.next_sequence
                || (!closed
                    && !matches!(
                        frame.record,
                        Record::Initialized(_) | Record::PayloadContinuation(_)
                    ))
            {
                return Err(invalid("candidate must contain one closed group"));
            }
            trial.push(&frame)?;
        }
        if decoder.tail() != Tail::Complete {
            return Err(invalid("candidate must contain one closed group"));
        }
        Ok(trial)
    }
    /// Actual retained extent capacity, counting both pending and closed summaries.
    pub fn retained_extent_capacity(&self) -> usize {
        self.summary.uncovered.capacity()
            + self
                .pending
                .as_ref()
                .map_or(0, |p| p.state.uncovered.capacity())
    }
    /// Actual retained catalog-parser heap; no catalog bytes are retained.
    pub fn retained_catalog_bytes(&self) -> usize {
        self.pending
            .as_ref()
            .and_then(|p| p.catalog.as_ref())
            .map_or(0, |c| c.validator.retained_bytes())
    }
    /// Consume a fully validated frame. Errors do not advance public authority.
    pub fn push(&mut self, frame: &Frame<'_>) -> Result<(), TapeLogError> {
        let result = if self.poisoned {
            Err(Fault::Invalid("fold poisoned"))
        } else {
            self.apply(frame)
        };
        result.map_err(|fault| {
            self.poisoned = true;
            TapeLogError {
                offset: frame.offset,
                sequence: Some(frame.sequence),
                fault,
            }
        })
    }
    /// Final summary and tail. An unclosed well-formed group is absent.
    pub fn finish(self, tail: Tail) -> Result<(TapeSummary, Tail), Fault> {
        if self.poisoned {
            return Err(Fault::Invalid("fold poisoned"));
        }
        let tail = if let Some(p) = self.pending {
            Tail::Torn {
                offset: p.group.reference.start_offset,
                kind: TornKind::UnclosedGroup {
                    zero_filled_at: match tail {
                        Tail::Torn {
                            offset,
                            kind: TornKind::ZeroFilled,
                        } => Some(offset),
                        _ => None,
                    },
                },
            }
        } else {
            tail
        };
        Ok((self.summary, tail))
    }
    fn publish_media(&mut self, mut state: TapeSummary) {
        let new_extents = std::mem::take(&mut state.uncovered);
        state.uncovered = std::mem::take(&mut self.summary.uncovered);
        let lower = state.protected;
        trim(&mut state, lower);
        state.uncovered.extend(new_extents);
        self.summary = state;
    }
    fn apply(&mut self, f: &Frame<'_>) -> Result<(), Fault> {
        if !f.payload_validated {
            return Err(Fault::Invalid("fold requires all section CRCs"));
        }
        if f.sequence != self.next_sequence {
            return Err(Fault::Sequence {
                expected: self.next_sequence,
                actual: f.sequence,
            });
        }
        self.next_sequence = add(f.sequence, 1)?;
        if !matches!(
            &f.record,
            Record::PayloadContinuation(Continuation {
                chunk: Chunk::Catalog { .. },
                ..
            })
        ) && !f.payload.is_empty()
        {
            return Err(Fault::Invalid("unexpected payload section"));
        }
        if self.summary.sealed {
            return Err(Fault::Invalid("record after seal"));
        }
        match &f.record {
            Record::Initialized(init) => {
                if self.summary.tape_uuid.is_some() || self.pending.is_some() {
                    return Err(Fault::SecondInitialized);
                }
                if f.sequence != 1
                    || init.group != f.sequence
                    || init.incarnation != 1
                    || init.tape_uuid == [0; 16]
                    || !remanence_parity::DEFAULT_BOOTSTRAP_CANDIDATE_BLOCK_SIZES
                        .contains(&init.block_size)
                {
                    return Err(Fault::Invalid("initialization identity/geometry"));
                }
                if let Some(s) = &init.scheme {
                    if s.id.as_str().len() > u16::MAX as usize {
                        return Err(Fault::Invalid("scheme ID exceeds existing header bound"));
                    }
                    s.validate().map_err(|_| Fault::Invalid("parity scheme"))?;
                }
                let mut p = Pending::new(&self.summary, f.sequence, f.offset);
                p.state.tape_uuid = Some(init.tape_uuid);
                p.state.incarnation = 1;
                p.state.block_size = init.block_size;
                p.state.scheme = init.scheme.clone();
                if init.bot.kind != TapeFileKind::Bootstrap
                    || init.bot.tape_file_number != 0
                    || init.bot.block_count != 1
                {
                    return Err(Fault::Invalid("BOT row"));
                }
                apply_row(&mut p, &init.bot, true)?;
                p.group.push(f)?;
                self.pending = Some(p);
                return Ok(());
            }
            Record::PayloadContinuation(c) => {
                if self.summary.is_fresh() && self.pending.is_none() {
                    return Err(Fault::Invalid("continuation before Initialized"));
                }
                let p = self
                    .pending
                    .get_or_insert_with(|| Pending::new(&self.summary, f.sequence, f.offset));
                if c.group != p.group.reference.first_sequence {
                    return Err(Fault::GroupMismatch);
                }
                match &c.chunk {
                    Chunk::FileRow(row) => {
                        if p.directory_count > 0 {
                            return Err(Fault::Invalid("file row after plan directory"));
                        }
                        apply_row(p, row, false)?;
                    }
                    Chunk::Catalog {
                        tape_file_number,
                        ordinal,
                        last,
                    } => {
                        let catalog = p
                            .catalog
                            .as_mut()
                            .ok_or(Fault::Invalid("catalog without Object row"))?;
                        if catalog.file != *tape_file_number
                            || catalog.ordinal != *ordinal
                            || f.payload.is_empty()
                        {
                            return Err(Fault::Invalid("catalog envelope/order"));
                        }
                        catalog.validator.feed(f.payload)?;
                        catalog.ordinal = add(catalog.ordinal, 1)?;
                        if *last {
                            p.members = add(p.members, catalog.validator.finish()?)?;
                            p.catalog = None;
                        }
                    }
                    Chunk::PlanDirectory(row) => {
                        if p.catalog.is_some()
                            || row.epoch_id != p.directory_count
                            || p.directory_file.is_some_and(|n| row.tape_file_number <= n)
                            || row.protected_ordinal_end_exclusive <= row.protected_ordinal_start
                            || row.sidecar_total_block_count == 0
                            || row.sidecar_header_block_count == 0
                            || row.parity_shard_block_count == 0
                            || row.flags & !7 != 0
                        {
                            return Err(Fault::Invalid("plan directory order/shape"));
                        }
                        identity(
                            &mut p.directory,
                            row.tape_file_number,
                            row.epoch_id,
                            row.protected_ordinal_start,
                            row.protected_ordinal_end_exclusive,
                            row.sidecar_total_block_count,
                            &row.canonical_metadata_hash,
                        );
                        p.directory_count = add(p.directory_count, 1)?;
                        p.directory_file = Some(row.tape_file_number);
                    }
                }
                p.group.push(f)?;
                return Ok(());
            }
            _ => {}
        }
        if self.summary.last_ordinary_commit.is_none() && !matches!(f.record, Record::Committed(_))
        {
            return Err(Fault::TerminalFirst);
        }
        let mut p = self
            .pending
            .take()
            .unwrap_or_else(|| Pending::new(&self.summary, f.sequence, f.offset));
        if p.catalog.is_some() {
            return Err(Fault::Invalid("closing incomplete catalog"));
        }
        match &f.record {
            Record::Committed(commit) => {
                if self.summary.finalization.is_some() {
                    return Err(Fault::Invalid(
                        "ordinary commit after finalization admission",
                    ));
                }
                if p.maps != 0 || p.components != 0 || p.directory_count != 0 {
                    return Err(Fault::Invalid("ordinary commit contains terminal rows"));
                }
                check_commit(&mut p, &self.summary, commit)?;
                p.state.last_ordinary_commit = Some(f.sequence);
                p.state.last_ordinary_record = Some(commit.clone());
                p.state.ordinary_count = add(p.state.ordinary_count, 1)?;
                media_commit(&mut p.state, f.sequence)?;
                self.publish_media(p.state);
            }
            Record::TerminalPrefix { decision, commit } => {
                let plan = accepted_plan(&self.summary, *decision)?;
                if plan.prefix_written
                    || plan.components_written != 0
                    || p.objects != 0
                    || p.components != 0
                    || p.directory_count != 0
                    || p.maps > 1
                    || p.rows != plan.prefix_rows
                    || digest(&p.row_digest) != plan.prefix_digest
                    || commit.cursor
                        != (CommitCursor {
                            partition: plan.layout.partition,
                            lba: plan.layout.components[0].planned_start_lba,
                        })
                {
                    return Err(Fault::Invalid("terminal prefix differs from plan"));
                }
                check_commit(&mut p, &self.summary, commit)?;
                p.state.finalization.as_mut().unwrap().prefix_written = true;
                media_commit(&mut p.state, f.sequence)?;
                self.publish_media(p.state);
            }
            Record::ComponentWritten {
                decision,
                component,
                commit,
            } => {
                let plan = accepted_plan(&self.summary, *decision)?;
                if !plan.prefix_written
                    || *component != plan.components_written
                    || *component >= 5
                    || p.rows != 1
                    || p.components != 1
                    || p.directory_count != 0
                {
                    return Err(Fault::Invalid("terminal component transition"));
                }
                let expected = plan.layout.components[*component as usize];
                if self.summary.next_file != expected.planned_tape_file_number
                    || self.summary.cursor.lba != expected.planned_start_lba
                    || commit.cursor.lba
                        != add(add(expected.planned_start_lba, expected.record_count)?, 1)?
                    || p.state.total != self.summary.total
                    || p.state.protected != self.summary.protected
                {
                    return Err(Fault::Invalid("terminal component layout"));
                }
                check_commit(&mut p, &self.summary, commit)?;
                if p.state.check != self.summary.check
                    || p.state.provenance != self.summary.provenance
                    || p.state.unverified_end != self.summary.unverified_end
                {
                    return Err(Fault::Invalid("component changed check"));
                }
                let progress = p.state.finalization.as_mut().unwrap();
                progress.components_written += 1;
                progress.recovery_required = false;
                media_commit(&mut p.state, f.sequence)?;
                self.publish_media(p.state);
            }
            Record::FinalizationDecided(plan) | Record::FinalizationPrepared(plan) => {
                check_reference(&p, &plan.group)?;
                let prepared = matches!(f.record, Record::FinalizationPrepared(_));
                if p.objects != 0
                    || p.components != 0
                    || p.maps > 1
                    || p.state.total != self.summary.total
                {
                    return Err(Fault::Invalid("plan contains non-prefix files"));
                }
                validate_plan(plan, &p, &self.summary, prepared)?;
                if let Some(previous) = &self.summary.finalization {
                    if !previous.prepared
                        || prepared
                        || previous.plan_identity != plan_identity(plan)?
                        || previous.layout != plan.layout
                        || previous.prefix_digest != digest(&p.row_digest)
                    {
                        return Err(Fault::Invalid("conflicting finalization plan"));
                    }
                }
                self.summary.finalization = Some(FinalizationState {
                    sequence: f.sequence,
                    reference: plan.group.clone(),
                    edition_digest: plan.edition_digest,
                    layout: plan.layout,
                    trigger: plan.trigger,
                    prepared,
                    recovery_required: false,
                    components_written: 0,
                    prefix_written: p.rows == 0,
                    plan_identity: plan_identity(plan)?,
                    prefix_digest: digest(&p.row_digest),
                    prefix_rows: p.rows,
                });
            }
            Record::Sealed {
                decision,
                cursor,
                edition_digest,
            } => {
                require_empty(&p)?;
                let plan = accepted_plan(&self.summary, *decision)?;
                if plan.components_written != 5
                    || *cursor != self.summary.cursor
                    || cursor.lba != plan.layout.expected_eod_lba
                    || *edition_digest != plan.edition_digest
                {
                    return Err(Fault::Invalid("seal before replica C or plan mismatch"));
                }
                self.summary.sealed = true;
                self.summary
                    .finalization
                    .as_mut()
                    .unwrap()
                    .recovery_required = false;
            }
            Record::FinalizationPreparationRetired { prepared } => {
                require_empty(&p)?;
                if !self
                    .summary
                    .finalization
                    .as_ref()
                    .is_some_and(|plan| plan.prepared && plan.sequence == *prepared)
                {
                    return Err(Fault::Invalid("retirement without preparation"));
                }
                self.summary.finalization = None;
            }
            Record::FinalizationRecoveryRequired { plan } => {
                require_empty(&p)?;
                let state = self
                    .summary
                    .finalization
                    .as_mut()
                    .ok_or(Fault::Invalid("recovery flag without plan"))?;
                if state.sequence != *plan {
                    return Err(Fault::Invalid("recovery plan reference"));
                }
                state.recovery_required = true;
            }
            _ => unreachable!(),
        }
        self.summary.last_closed_sequence = f.sequence;
        self.summary.closed_end = f.end;
        Ok(())
    }
}
fn add(a: u64, b: u64) -> Result<u64, Fault> {
    a.checked_add(b).ok_or(Fault::Overflow)
}
fn digest(h: &Sha256) -> [u8; 32] {
    h.clone().finalize().into()
}
fn check_reference(p: &Pending, r: &GroupRef) -> Result<(), Fault> {
    if p.reference() != *r {
        Err(Fault::GroupMismatch)
    } else {
        Ok(())
    }
}
fn require_empty(p: &Pending) -> Result<(), Fault> {
    if p.group.reference.frame_count != 0 {
        Err(Fault::GroupMismatch)
    } else {
        Ok(())
    }
}
fn media_commit(s: &mut TapeSummary, seq: u64) -> Result<(), Fault> {
    s.last_media_commit = Some(seq);
    s.barrier_count = add(s.barrier_count, 1)?;
    Ok(())
}
fn accepted_plan(s: &TapeSummary, seq: u64) -> Result<&FinalizationState, Fault> {
    s.finalization
        .as_ref()
        .filter(|p| p.sequence == seq && !p.prepared)
        .ok_or(Fault::Invalid("no accepted writable plan"))
}
fn identity(
    h: &mut Sha256,
    file: u64,
    epoch: u64,
    start: u64,
    end: u64,
    blocks: u64,
    hash: &[u8; 32],
) {
    for n in [file, epoch, start, end, blocks] {
        h.update(n.to_le_bytes());
    }
    h.update(hash);
}
fn trim(s: &mut TapeSummary, lower: u64) {
    while s.uncovered.front().is_some_and(|e| e.end <= lower) {
        s.uncovered.pop_front();
    }
    if let Some(e) = s.uncovered.front_mut() {
        if e.first < lower {
            e.start_lba += lower - e.first;
            e.first = lower;
        }
    }
}
fn apply_row(p: &mut Pending, row: &TapeFileEntry, bot: bool) -> Result<(), Fault> {
    if p.catalog.is_some() {
        return Err(Fault::Invalid("Object without complete catalog"));
    }
    let s = &mut p.state;
    if row.tape_file_number != s.next_file
        || row.block_count == 0
        || row.physical_start_hint != Some(s.cursor.lba)
    {
        return Err(Fault::Invalid("dense rows or cursor/extent agreement"));
    }
    if !matches!(row.kind, TapeFileKind::Object)
        && (row.object_id.is_some()
            || row.object_recovery_row.is_some()
            || row.first_parity_data_ordinal.is_some())
    {
        return Err(Fault::Invalid("non-Object recovery fields"));
    }
    if row.kind != TapeFileKind::ParitySidecar
        && (row.epoch_id.is_some()
            || row.protected_ordinal_start.is_some()
            || row.protected_ordinal_end_exclusive.is_some())
    {
        return Err(Fault::Invalid("non-sidecar epoch fields"));
    }
    match row.kind {
        TapeFileKind::Bootstrap => {
            if !bot {
                return Err(Fault::Invalid("second BOT row"));
            }
        }
        TapeFileKind::Object => {
            if s.seal_due.is_some() {
                return Err(Fault::Invalid("Object after seal due"));
            }
            if s.finalization.is_some() {
                return Err(Fault::Invalid("Object after finalization"));
            }
            let id = row
                .object_id
                .as_ref()
                .filter(|s| !s.is_empty() && s.len() <= 64)
                .ok_or(Fault::Invalid("Object identity"))?;
            let recovery = row
                .object_recovery_row
                .as_ref()
                .ok_or(Fault::Invalid("Object recovery row missing"))?;
            if recovery.tape_file_number != row.tape_file_number
                || recovery.stored_block_count != row.block_count
                || recovery.object_id.as_deref() != Some(id.as_bytes())
            {
                return Err(Fault::Invalid("Object recovery identity"));
            }
            remanence_parity::object_recovery::validate_object_recovery_row_fields(
                recovery.stored_block_count,
                recovery.object_id.as_deref(),
                &recovery.representation,
                Some(s.block_size),
            )
            .map_err(|_| Fault::Invalid("Object recovery anchors"))?;
            if s.scheme.is_some() {
                if row.first_parity_data_ordinal != Some(s.total) {
                    return Err(Fault::Invalid("Object data ordinal"));
                }
                let end = add(s.total, row.block_count)?;
                let lower = s.protected.max(end.saturating_sub(s.epoch_blocks() - 1));
                trim(s, lower);
                let first = s.total.max(lower);
                s.uncovered.push_back(UncoveredExtent {
                    first,
                    end,
                    tape_file_number: row.tape_file_number,
                    start_lba: add(s.cursor.lba, first - s.total)?,
                });
                s.total = end;
            } else {
                if row.first_parity_data_ordinal.is_some() {
                    return Err(Fault::Invalid("parity-off ordinal"));
                }
                s.total = add(s.total, row.block_count)?;
            }
            s.object_count = add(s.object_count, 1)?;
            p.objects = add(p.objects, 1)?;
            p.catalog = Some(Catalog {
                file: row.tape_file_number,
                ordinal: 0,
                validator: CatalogValidator::new(row, s.tape_uuid.unwrap(), s.block_size, s.total)?,
            });
        }
        TapeFileKind::ParitySidecar => {
            let end = row
                .protected_ordinal_end_exclusive
                .ok_or(Fault::Invalid("sidecar coverage"))?;
            if s.scheme.is_none()
                || row.epoch_id != Some(s.next_epoch)
                || row.protected_ordinal_start != Some(s.protected)
                || end <= s.protected
                || end > s.total
                || end - s.protected > s.epoch_blocks()
            {
                return Err(Fault::Invalid("sidecar coverage/epoch"));
            }
            let hash = row
                .canonical_metadata_hash
                .ok_or(Fault::Invalid("sidecar identity hash"))?;
            identity(
                &mut s.sidecar_identity,
                row.tape_file_number,
                s.next_epoch,
                s.protected,
                end,
                row.block_count,
                &hash,
            );
            s.protected = end;
            s.next_epoch = add(s.next_epoch, 1)?;
            trim(s, end);
        }
        TapeFileKind::ParityMap => {
            if row.canonical_metadata_hash.is_none() || s.scheme.is_none() || p.maps != 0 {
                return Err(Fault::Invalid("ParityMap identity"));
            }
            p.maps = add(p.maps, 1)?;
        }
        TapeFileKind::TapeIndexReplica | TapeFileKind::IndexSeparationExtent => {
            let plan = s
                .finalization
                .as_ref()
                .ok_or(Fault::Invalid("component before decision"))?;
            let component = plan
                .layout
                .components
                .get(plan.components_written as usize)
                .ok_or(Fault::Invalid("extra component"))?;
            let expected = match component.kind {
                TerminalTailComponentKind::TapeIndexReplica => TapeFileKind::TapeIndexReplica,
                TerminalTailComponentKind::IndexSeparationExtent => {
                    TapeFileKind::IndexSeparationExtent
                }
            };
            if row.kind != expected || row.block_count != component.record_count {
                return Err(Fault::Invalid("component kind/size"));
            }
            p.components = add(p.components, 1)?;
        }
    }
    if p.maps > 0 && row.kind != TapeFileKind::ParityMap {
        return Err(Fault::Invalid("file after ParityMap"));
    }
    s.cursor.lba = add(add(s.cursor.lba, row.block_count)?, 1)?;
    s.next_file = add(s.next_file, 1)?;
    if !bot {
        p.rows = add(p.rows, 1)?;
        let mut bytes = Vec::new();
        super::wire::Wire::put(row, &mut bytes).map_err(|_| Fault::Invalid("row encoding"))?;
        p.row_digest.update(bytes);
    }
    Ok(())
}
fn check_commit(p: &mut Pending, previous: &TapeSummary, c: &Committed) -> Result<(), Fault> {
    check_reference(p, &c.group)?;
    let s = &mut p.state;
    if s.is_fresh()
        || c.cursor != s.cursor
        || c.next_file != s.next_file
        || c.next_epoch != s.next_epoch
        || c.object_count != s.object_count
        || c.protected != s.protected
        || c.total != s.total
        || c.protected < previous.protected
        || c.total < previous.total
    {
        return Err(Fault::Invalid("commit counters/cursor"));
    }
    if c.protected > c.total || (s.scheme.is_some() && c.total - c.protected >= s.epoch_blocks()) {
        return Err(Fault::Invalid("uncovered range exceeds epoch"));
    }
    if s.scheme.is_none()
        && (c.protected != 0 || !s.uncovered.is_empty() || c.unverified_end.is_some())
    {
        return Err(Fault::Invalid("parity-off state"));
    }
    if s.scheme.is_none() || c.total == c.protected {
        if c.check != CheckValue::EMPTY || c.provenance != CheckProvenance::Empty {
            return Err(Fault::Invalid("nonempty check for empty range"));
        }
    } else {
        if c.provenance == CheckProvenance::Empty {
            return Err(Fault::Invalid("empty provenance for uncovered data"));
        }
        trim(s, c.protected);
        // Old extents cover [previous.W, previous.T); inspect only the new
        // suffix here. Merge/prune the old deque only after every check passes.
        if c.total > previous.total
            && (s.uncovered.front().map(|e| e.first) != Some(c.protected.max(previous.total))
                || s.uncovered.back().map(|e| e.end) != Some(c.total))
        {
            return Err(Fault::Invalid("missing uncovered extents"));
        }
    }
    match (c.provenance, c.unverified_end) {
        (CheckProvenance::UnverifiedRebuild, Some(end)) if c.protected < end && end <= c.total => {}
        (CheckProvenance::Empty | CheckProvenance::OriginalWrite, None) => {}
        _ => return Err(Fault::Invalid("unverified range/provenance")),
    }
    if let Some(end) = previous.unverified_end.filter(|end| c.protected < *end) {
        if c.unverified_end.is_none_or(|next| next < end) {
            return Err(Fault::Invalid("unverified rebuild promoted"));
        }
    }
    if previous.seal_due.is_some() && c.seal_due.is_none() {
        return Err(Fault::Invalid("lost SealDueReason"));
    }
    s.check = c.check;
    s.provenance = c.provenance;
    s.unverified_end = c.unverified_end;
    s.seal_due = c.seal_due;
    Ok(())
}
fn validate_plan(
    plan: &FinalizationPlan,
    p: &Pending,
    s: &TapeSummary,
    prepared: bool,
) -> Result<(), Fault> {
    plan.layout
        .validate()
        .map_err(|_| Fault::Invalid("terminal layout"))?;
    let first = plan.layout.components[0];
    if plan.layout.block_size != s.block_size
        || plan.layout.partition != s.cursor.partition
        || plan.prefix_end != p.state.cursor
        || first.planned_start_lba != plan.prefix_end.lba
        || first.planned_tape_file_number != p.state.next_file
        || plan.edition_id == [0; 16]
        || plan.edition_digest == [0; 32]
        || plan.edition_sequence == 0
        || (s.scheme.is_some() && p.state.protected != p.state.total)
    {
        return Err(Fault::Invalid("terminal plan binding"));
    }
    if plan.writer_version.len() > 128
        || !plan.writer_version.bytes().all(|b| (32..=126).contains(&b))
        || plan.write_timestamp.len() > 64
        || time::OffsetDateTime::parse(
            &plan.write_timestamp,
            &time::format_description::well_known::Rfc3339,
        )
        .is_err()
    {
        return Err(Fault::Invalid("plan diagnostics"));
    }
    if prepared && plan.manual.is_none() {
        return Err(Fault::Invalid("automatic plan cannot be provisional"));
    }
    if let Some(m) = &plan.manual {
        if plan.trigger != TerminalFinalizationTrigger::OperatorCloseOut
            || m.operation_id == [0; 16]
            || m.idempotency_key == [0; 16]
            || m.request_fingerprint == [0; 32]
            || m.operation_kind.is_empty()
            || m.actor_fingerprint.is_empty()
            || m.reason.is_empty()
            || m.assigned_pool_id != m.expected_pool_id
        {
            return Err(Fault::Invalid("manual acceptance identity"));
        }
    } else if plan.trigger == TerminalFinalizationTrigger::OperatorCloseOut {
        return Err(Fault::Invalid("manual identity absent"));
    }
    if let Some(reason) = s.seal_due {
        let trigger = match reason {
            SealDueReason::HardwareEarlyWarning => {
                TerminalFinalizationTrigger::HardwareEarlyWarning
            }
            SealDueReason::ReachedLowWatermark => TerminalFinalizationTrigger::ReachedLowWatermark,
        };
        if plan.manual.is_none() && plan.trigger != trigger {
            return Err(Fault::Invalid("automatic trigger lost seal reason"));
        }
    }
    if (p.maps == 1) != (p.directory_count > 0) {
        return Err(Fault::Invalid("ParityMap presence/directory"));
    }
    if p.directory_count != p.state.next_epoch
        || digest(&p.directory) != digest(&p.state.sidecar_identity)
    {
        return Err(Fault::Invalid("plan directory identity/coverage"));
    }
    Ok(())
}
/// Full trust-validation and fold over a Read stream, with O(E+F) retained memory.
pub fn replay(reader: impl Read) -> Result<(TapeSummary, Tail), CodecError> {
    let mut decoder = Decoder::new(reader);
    let mut fold = Fold::new();
    while let Some(frame) = decoder.next_frame()? {
        fold.push(&frame)?;
    }
    fold.finish(decoder.tail()).map_err(|fault| {
        TapeLogError {
            offset: 0,
            sequence: None,
            fault,
        }
        .into()
    })
}
/// Pure semantic tail classifier over bytes, including unclosed groups.
pub fn classify(bytes: &[u8]) -> Result<Tail, CodecError> {
    replay(bytes).map(|(_, tail)| tail)
}

// A preparation can only become the identical accepted plan, including operator
// identity and diagnostics. The group's byte location necessarily changes.
fn plan_identity(plan: &FinalizationPlan) -> Result<[u8; 32], Fault> {
    use super::wire::Wire;
    struct Hasher(Sha256);
    impl std::io::Write for Hasher {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut out = Hasher(Sha256::new());
    macro_rules! field { ($($field:ident),+) => {$(plan.$field.put(&mut out).map_err(|_|Fault::Invalid("plan identity encoding"))?;)+}; }
    field!(
        trigger,
        manual,
        edition_id,
        edition_sequence,
        edition_digest,
        writer_version,
        write_timestamp,
        layout,
        prefix_end
    );
    Ok(digest(&out.0))
}
