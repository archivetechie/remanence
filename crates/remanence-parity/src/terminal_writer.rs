//! Restart-safe writer for the five-file terminal tape-index tail.
//!
//! This layer streams already-planned replica/gap codecs to a raw tape sink.
//! It deliberately does not own the checkpoint database. Instead, a durable
//! authority callback supplies physical reconciliation evidence and fsyncs the
//! component journal/progress transition after the writer proves the trailing
//! filemark, zero-count barrier, and observed position.

use crate::error::ParityError;
use crate::filemark_map::TapeFileKind;
use crate::index_separation::{
    parse_index_separation_footer, parse_index_separation_header, plan_index_separation,
    validate_index_separation_full, write_index_separation, IndexSeparationDescriptor,
    IndexSeparationError, IndexSeparationInteriorBlockSource, IndexSeparationObservation,
    IndexSeparationPlan, DEFAULT_INDEX_SEPARATION_BYTES,
};
use crate::journal::{CommittedBundle, CommittedBundleKind, TapeFileEntry};
use crate::raw::{
    raw_read_error_proves_damage, PhysicalPositionHint, RawReadOutcome, RawTapeSink, RawTapeSource,
    RawWriteOutcome,
};
use crate::tape_index_replica::{
    parse_tape_index_bootstrap_footer, parse_tape_index_replica_header, plan_tape_index_replica,
    validate_tape_index_replica_payload, write_tape_index_replica, TapeIndexEditionPlan,
    TapeIndexReplicaError, TapeIndexReplicaObservation, TapeIndexReplicaPayloadBlockSource,
    TapeIndexReplicaPlan, TapeIndexReplicaRecordSource,
};
use crate::terminal_tail::{
    TerminalTailComponentKind, TerminalTailComponentPlan, TerminalTailLayoutError,
    TerminalTailProgress, TERMINAL_TAIL_COMPONENT_COUNT,
};

/// Complete immutable codec plan for A/gap-AB/B/gap-BC/C.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalTripleWritePlan {
    /// Common final edition and payload authority digest.
    pub edition: TapeIndexEditionPlan,
    /// Replica-local A/B/C envelopes.
    pub replicas: [TapeIndexReplicaPlan; 3],
    /// Typed separation extents AB/BC.
    pub separations: [IndexSeparationPlan; 2],
}

impl TerminalTripleWritePlan {
    /// Bind the three replicas and two default one-GiB separation extents.
    pub fn new(edition: TapeIndexEditionPlan) -> Result<Self, TerminalTailWriteError> {
        let replicas = [
            plan_tape_index_replica(edition.clone(), 1)?,
            plan_tape_index_replica(edition.clone(), 2)?,
            plan_tape_index_replica(edition.clone(), 3)?,
        ];
        let separation = |gap_ordinal| {
            plan_index_separation(IndexSeparationDescriptor {
                tape_uuid: edition.descriptor.tape_uuid,
                edition_id: edition.descriptor.edition_id,
                gap_ordinal,
                block_size: edition.descriptor.block_size,
                nominal_extent_bytes: DEFAULT_INDEX_SEPARATION_BYTES,
                total_records: edition
                    .descriptor
                    .terminal_layout
                    .separation(gap_ordinal)?
                    .record_count,
                compression_enabled: edition.descriptor.compression_enabled,
                terminal_layout: edition.descriptor.terminal_layout,
            })
            .map_err(TerminalTailWriteError::from)
        };
        let separations = [separation(1)?, separation(2)?];
        Self::from_parts(edition, replicas, separations)
    }

    /// Assemble an explicit plan, useful for profiles with non-default gaps.
    pub fn from_parts(
        edition: TapeIndexEditionPlan,
        replicas: [TapeIndexReplicaPlan; 3],
        separations: [IndexSeparationPlan; 2],
    ) -> Result<Self, TerminalTailWriteError> {
        edition.descriptor.terminal_layout.validate()?;
        for (index, replica) in replicas.iter().enumerate() {
            let ordinal = u16::try_from(index + 1).expect("three replica ordinals fit u16");
            let expected = plan_tape_index_replica(edition.clone(), ordinal)?;
            if replica != &expected {
                return Err(TerminalTailWriteError::PlanMismatch(
                    "replica does not match its canonical common-edition plan",
                ));
            }
        }
        for (index, separation) in separations.iter().enumerate() {
            let ordinal = u16::try_from(index + 1).expect("two gap ordinals fit u16");
            let expected = plan_index_separation(separation.descriptor)?;
            if separation != &expected
                || separation.descriptor.tape_uuid != edition.descriptor.tape_uuid
                || separation.descriptor.edition_id != edition.descriptor.edition_id
                || separation.descriptor.terminal_layout != edition.descriptor.terminal_layout
                || separation.descriptor.gap_ordinal != ordinal
            {
                return Err(TerminalTailWriteError::PlanMismatch(
                    "separation does not match its canonical common-edition plan",
                ));
            }
        }
        Ok(Self {
            edition,
            replicas,
            separations,
        })
    }

    /// Revalidate all public plan fields against canonical codec planning.
    pub fn validate(&self) -> Result<(), TerminalTailWriteError> {
        let canonical = Self::from_parts(
            self.edition.clone(),
            self.replicas.clone(),
            self.separations,
        )?;
        if &canonical != self {
            return Err(TerminalTailWriteError::PlanMismatch(
                "terminal triple differs from canonical immutable plan",
            ));
        }
        Ok(())
    }

    fn component(&self, index: usize) -> TerminalTailComponentPlan {
        self.edition.descriptor.terminal_layout.components[index]
    }
}

/// Physical-tail fact established by bounded reconciliation before a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalComponentReconcileEvidence {
    /// No bytes for the next component exist and the cursor is at its plan.
    Absent,
    /// The complete component and its trailing filemark already exist.
    Complete,
    /// A partial component exists and its immutable start is proved
    /// overwritable. The writer locates to that start and rewrites this and
    /// every later component through the normal five-step state machine.
    TornRewritable,
    /// A partial component exists on WORM media.
    TornWorm,
    /// The component start or physical tail could not be proved.
    Unproved,
}

/// Fully validate and classify the only legal next terminal component.
///
/// The walk is bounded by the immutable component record count. `Complete`
/// validates the exact header, full payload/interior, footer, placement, and
/// trailing filemark and leaves the read cursor after that filemark. `Absent`
/// proves EOD at the component start and restores that cursor. A proved torn
/// result also restores the component-start cursor when possible; the writer
/// independently locates its write sink before overwrite.
pub fn reconcile_terminal_tail_next(
    source: &mut dyn RawTapeSource,
    plan: &TerminalTripleWritePlan,
    progress: TerminalTailProgress,
    rewritable: bool,
) -> TerminalComponentReconcileEvidence {
    if plan.validate().is_err() {
        return TerminalComponentReconcileEvidence::Unproved;
    }
    let Some(component_index) = progress.next_component_index() else {
        return TerminalComponentReconcileEvidence::Complete;
    };
    let component = plan.component(component_index);
    let start = PhysicalPositionHint {
        partition: plan.edition.descriptor.terminal_layout.partition,
        lba: component.planned_start_lba,
    };
    if source
        .configure_fixed_block_size(plan.edition.descriptor.block_size)
        .and_then(|()| source.locate_physical(start))
        .is_err()
    {
        return TerminalComponentReconcileEvidence::Unproved;
    }
    let block_size = match usize::try_from(plan.edition.descriptor.block_size) {
        Ok(size) => size,
        Err(_) => return TerminalComponentReconcileEvidence::Unproved,
    };
    let mut header_block = vec![0; block_size];
    match source.read_record(&mut header_block) {
        Ok(RawReadOutcome::EndOfData { position_after })
            if position_after.partition == start.partition && position_after.lba == start.lba =>
        {
            if source.locate_physical(start).is_err() {
                return TerminalComponentReconcileEvidence::Unproved;
            }
            return TerminalComponentReconcileEvidence::Absent;
        }
        Ok(RawReadOutcome::Block { bytes, .. }) if bytes == block_size => {}
        Ok(_) => return classify_torn_at_start(source, start, rewritable),
        Err(error) => return classify_tail_read_error(source, start, rewritable, &error),
    }

    let footer_lba =
        match checked_component_footer_lba(component.planned_start_lba, component.record_count) {
            Some(lba) => lba,
            None => return TerminalComponentReconcileEvidence::Unproved,
        };
    let footer_position = PhysicalPositionHint {
        partition: start.partition,
        lba: footer_lba,
    };
    let mut footer_block = vec![0; block_size];
    if source.locate_physical(footer_position).is_err() {
        return TerminalComponentReconcileEvidence::Unproved;
    }
    match source.read_record(&mut footer_block) {
        Ok(RawReadOutcome::Block { bytes, .. }) if bytes == block_size => {}
        Ok(_) => return classify_torn_at_start(source, start, rewritable),
        Err(error) => return classify_tail_read_error(source, start, rewritable, &error),
    }

    let validation = match component.kind {
        TerminalTailComponentKind::TapeIndexReplica => {
            validate_physical_replica(source, plan, component_index, &header_block, &footer_block)
        }
        TerminalTailComponentKind::IndexSeparationExtent => validate_physical_separation(
            source,
            plan,
            component_index,
            &header_block,
            &footer_block,
        ),
    };
    match validation {
        PhysicalComponentValidation::Valid => {}
        PhysicalComponentValidation::Invalid => {
            return classify_torn_at_start(source, start, rewritable);
        }
        PhysicalComponentValidation::Unproved => {
            return TerminalComponentReconcileEvidence::Unproved;
        }
    }

    let filemark_position = PhysicalPositionHint {
        partition: start.partition,
        lba: match component
            .planned_start_lba
            .checked_add(component.record_count)
        {
            Some(lba) => lba,
            None => return TerminalComponentReconcileEvidence::Unproved,
        },
    };
    if source.locate_physical(filemark_position).is_err() {
        return TerminalComponentReconcileEvidence::Unproved;
    }
    let Some(expected_position_after_lba) = filemark_position.lba.checked_add(1) else {
        return TerminalComponentReconcileEvidence::Unproved;
    };
    match source.read_record(&mut header_block) {
        Ok(RawReadOutcome::Filemark { position_after })
            if position_after.partition == start.partition
                && position_after.lba == expected_position_after_lba =>
        {
            TerminalComponentReconcileEvidence::Complete
        }
        Ok(_) => classify_torn_at_start(source, start, rewritable),
        Err(error) => classify_tail_read_error(source, start, rewritable, &error),
    }
}

fn checked_component_footer_lba(start_lba: u64, record_count: u64) -> Option<u64> {
    start_lba.checked_add(record_count.checked_sub(1)?)
}

fn classify_torn_at_start(
    source: &mut dyn RawTapeSource,
    start: PhysicalPositionHint,
    rewritable: bool,
) -> TerminalComponentReconcileEvidence {
    if source.locate_physical(start).is_err() {
        return TerminalComponentReconcileEvidence::Unproved;
    }
    if rewritable {
        TerminalComponentReconcileEvidence::TornRewritable
    } else {
        TerminalComponentReconcileEvidence::TornWorm
    }
}

fn classify_tail_read_error(
    source: &mut dyn RawTapeSource,
    start: PhysicalPositionHint,
    rewritable: bool,
    error: &ParityError,
) -> TerminalComponentReconcileEvidence {
    if raw_read_error_proves_damage(error) {
        classify_torn_at_start(source, start, rewritable)
    } else {
        TerminalComponentReconcileEvidence::Unproved
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhysicalComponentValidation {
    Valid,
    Invalid,
    Unproved,
}

fn validate_physical_replica(
    source: &mut dyn RawTapeSource,
    plan: &TerminalTripleWritePlan,
    component_index: usize,
    header_block: &[u8],
    footer_block: &[u8],
) -> PhysicalComponentValidation {
    let expected = &plan.replicas[component_index / 2];
    let Ok(header) =
        parse_tape_index_replica_header(header_block, &plan.edition.descriptor.tape_uuid)
    else {
        return PhysicalComponentValidation::Invalid;
    };
    let Ok(footer) =
        parse_tape_index_bootstrap_footer(footer_block, &plan.edition.descriptor.tape_uuid)
    else {
        return PhysicalComponentValidation::Invalid;
    };
    if &header.plan != expected || &footer.plan != expected {
        return PhysicalComponentValidation::Invalid;
    }
    let Some(payload_start_lba) = expected.component.planned_start_lba.checked_add(1) else {
        return PhysicalComponentValidation::Invalid;
    };
    let payload_start = PhysicalPositionHint {
        partition: plan.edition.descriptor.terminal_layout.partition,
        lba: payload_start_lba,
    };
    if source.locate_physical(payload_start).is_err() {
        return PhysicalComponentValidation::Unproved;
    }
    let mut blocks = RawReplicaPayload {
        source,
        remaining: expected.edition.replica_layout.payload_record_count,
        block_size: expected.edition.descriptor.block_size as usize,
        unproved_read: false,
    };
    let result =
        validate_tape_index_replica_payload(&header, &footer, &mut blocks, |_| Ok(()), |_| Ok(()));
    match result {
        Ok(_) => PhysicalComponentValidation::Valid,
        Err(_) if blocks.unproved_read => PhysicalComponentValidation::Unproved,
        Err(_) => PhysicalComponentValidation::Invalid,
    }
}

struct RawReplicaPayload<'a> {
    source: &'a mut dyn RawTapeSource,
    remaining: u64,
    block_size: usize,
    unproved_read: bool,
}

impl TapeIndexReplicaPayloadBlockSource for RawReplicaPayload<'_> {
    fn visit_payload_blocks(
        &mut self,
        visitor: &mut dyn FnMut(&[u8]) -> Result<(), TapeIndexReplicaError>,
    ) -> Result<(), TapeIndexReplicaError> {
        let mut block = vec![0; self.block_size];
        for _ in 0..self.remaining {
            match self.source.read_record(&mut block) {
                Ok(RawReadOutcome::Block { bytes, .. }) if bytes == self.block_size => {
                    visitor(&block)?;
                }
                Ok(_) => {
                    return Err(TapeIndexReplicaError::Payload {
                        message: "physical payload is torn or unreadable".to_string(),
                    });
                }
                Err(error) => {
                    self.unproved_read = !raw_read_error_proves_damage(&error);
                    return Err(TapeIndexReplicaError::Payload {
                        message: "physical payload is torn or unreadable".to_string(),
                    });
                }
            }
        }
        Ok(())
    }
}

fn validate_physical_separation(
    source: &mut dyn RawTapeSource,
    plan: &TerminalTripleWritePlan,
    component_index: usize,
    header_block: &[u8],
    footer_block: &[u8],
) -> PhysicalComponentValidation {
    let expected = &plan.separations[component_index / 2];
    let Ok(header) =
        parse_index_separation_header(header_block, &plan.edition.descriptor.tape_uuid)
    else {
        return PhysicalComponentValidation::Invalid;
    };
    let Ok(footer) =
        parse_index_separation_footer(footer_block, &plan.edition.descriptor.tape_uuid)
    else {
        return PhysicalComponentValidation::Invalid;
    };
    if &header.plan != expected || &footer.plan != expected {
        return PhysicalComponentValidation::Invalid;
    }
    let Some(interior_start_lba) = expected.component.planned_start_lba.checked_add(1) else {
        return PhysicalComponentValidation::Invalid;
    };
    let Some(interior_record_count) = expected.descriptor.total_records.checked_sub(2) else {
        return PhysicalComponentValidation::Invalid;
    };
    let interior_start = PhysicalPositionHint {
        partition: plan.edition.descriptor.terminal_layout.partition,
        lba: interior_start_lba,
    };
    if source.locate_physical(interior_start).is_err() {
        return PhysicalComponentValidation::Unproved;
    }
    let mut blocks = RawSeparationInterior {
        source,
        remaining: interior_record_count,
        block_size: expected.descriptor.block_size as usize,
        unproved_read: false,
    };
    let result = validate_index_separation_full(&header, &footer, &mut blocks);
    match result {
        Ok(_) => PhysicalComponentValidation::Valid,
        Err(_) if blocks.unproved_read => PhysicalComponentValidation::Unproved,
        Err(_) => PhysicalComponentValidation::Invalid,
    }
}

struct RawSeparationInterior<'a> {
    source: &'a mut dyn RawTapeSource,
    remaining: u64,
    block_size: usize,
    unproved_read: bool,
}

impl IndexSeparationInteriorBlockSource for RawSeparationInterior<'_> {
    fn visit_interior_blocks(
        &mut self,
        visitor: &mut dyn FnMut(&[u8]) -> Result<(), IndexSeparationError>,
    ) -> Result<(), IndexSeparationError> {
        let mut block = vec![0; self.block_size];
        for _ in 0..self.remaining {
            match self.source.read_record(&mut block) {
                Ok(RawReadOutcome::Block { bytes, .. }) if bytes == self.block_size => {
                    visitor(&block)?;
                }
                Ok(_) => {
                    return Err(IndexSeparationError::PhysicalSource(
                        "physical separation is torn or unreadable".to_string(),
                    ));
                }
                Err(error) => {
                    self.unproved_read = !raw_read_error_proves_damage(&error);
                    return Err(IndexSeparationError::PhysicalSource(
                        "physical separation is torn or unreadable".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Barrier-proved component evidence supplied to the durable authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalComponentCommit {
    /// Durable progress before this transition.
    pub previous_progress: TerminalTailProgress,
    /// Durable progress after this transition.
    pub next_progress: TerminalTailProgress,
    /// Exact planned component.
    pub component: TerminalTailComponentPlan,
    /// Position returned by the successful zero-count barrier and re-read.
    pub observed_position: PhysicalPositionHint,
    /// True when restart reused an already-complete component.
    pub reconciled_existing_component: bool,
    /// True when a proved torn component was overwritten from its immutable
    /// start on rewritable media.
    pub rewrote_torn_component: bool,
    /// Exact sink-journal record the callback must durably reconcile/append.
    pub journal_bundle: CommittedBundle,
    /// Canonical empty watermark record the callback appends immediately after
    /// [`Self::journal_bundle`] and before advancing checkpoint progress.
    pub checkpoint_bundle: CommittedBundle,
}

/// Durable progress and physical-reconciliation seam.
pub trait TerminalTailAuthority {
    /// Load the current fsynced component progress.
    fn load_progress(&mut self) -> Result<TerminalTailProgress, String>;

    /// Reconcile the only legal next component against physical media.
    /// `Complete` means the full typed body/footer/filemark was independently
    /// validated against this exact component plan, not merely that the cursor
    /// happens to be at its planned end.
    fn reconcile_next(
        &mut self,
        progress: TerminalTailProgress,
        component: TerminalTailComponentPlan,
    ) -> Result<TerminalComponentReconcileEvidence, String>;

    /// Fsync sink-journal authority and advance durable component progress.
    ///
    /// If the sink bundle already exists as orphan evidence, implementations
    /// reconcile it rather than appending a duplicate record. It must make the
    /// supplied component bundle and following canonical checkpoint bundle
    /// durable, in that order, before advancing progress. The method returns
    /// only after the new progress is durable.
    fn commit_after_barrier(&mut self, commit: &TerminalComponentCommit) -> Result<(), String>;
}

/// Successful one-step result or a typed no-motion recovery outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalTailStepOutcome {
    /// One component gained durable barrier authority.
    Advanced(TerminalComponentCommit),
    /// All five planned components were already durable; no media command ran.
    AlreadyComplete,
    /// Continuation cannot safely append at the observed tail.
    RecoveryRequired {
        /// Last durable component authority.
        progress: TerminalTailProgress,
        /// Component whose physical state is unresolved.
        component: TerminalTailComponentPlan,
        /// Reconciliation classification.
        evidence: TerminalComponentReconcileEvidence,
    },
}

/// Run-to-completion result for the terminal tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalTailRunOutcome {
    /// All five components have durable barrier authority.
    Complete,
    /// Continuation stopped before motion because reconciliation was unsafe.
    RecoveryRequired {
        /// Last durable component authority.
        progress: TerminalTailProgress,
        /// Component whose physical state is unresolved.
        component: TerminalTailComponentPlan,
        /// Reconciliation classification.
        evidence: TerminalComponentReconcileEvidence,
    },
}

/// Terminal writer failure. A media/authority failure never advances progress.
#[derive(Debug, thiserror::Error)]
pub enum TerminalTailWriteError {
    /// Shared terminal layout is invalid.
    #[error("terminal writer layout error: {0}")]
    Layout(#[from] TerminalTailLayoutError),
    /// Replica codec/planning failed.
    #[error("terminal writer replica error: {0}")]
    Replica(#[from] TapeIndexReplicaError),
    /// Separation codec/planning failed.
    #[error("terminal writer separation error: {0}")]
    Separation(#[from] IndexSeparationError),
    /// Raw tape operation failed; completion may be unknown.
    #[error("terminal writer media error: {0}")]
    Media(#[from] ParityError),
    /// Immutable component plans disagree.
    #[error("terminal writer plan mismatch: {0}")]
    PlanMismatch(&'static str),
    /// Durable authority callback failed.
    #[error("terminal writer authority error: {0}")]
    Authority(String),
    /// Device position differs from the immutable plan.
    #[error(
        "terminal writer observed partition {actual_partition} lba {actual_lba}, expected partition {expected_partition} lba {expected_lba}"
    )]
    PositionMismatch {
        /// Planned partition.
        expected_partition: u32,
        /// Planned LBA.
        expected_lba: u64,
        /// Observed partition.
        actual_partition: u32,
        /// Observed LBA.
        actual_lba: u64,
    },
    /// A successful fixed-record write reported a short completion.
    #[error("terminal writer short block completion: wrote {actual} bytes, expected {expected}")]
    ShortWrite {
        /// Fixed block size.
        expected: u32,
        /// Completion byte count.
        actual: u32,
    },
    /// The drive reported EOM despite the pre-proved close reserve.
    #[error("terminal writer reached end of medium during reserved close")]
    EndOfMedium,
}

/// Stream as many restart-safe components as reconciliation permits.
///
/// Each component receives its own filemark, synchronous zero-count barrier,
/// position proof, and durable authority transition before the next component
/// is considered. A recovery-required classification returns without issuing
/// any media command for that component.
pub fn write_terminal_tail<S, A>(
    sink: &mut dyn RawTapeSink,
    source: &mut S,
    authority: &mut A,
    plan: &TerminalTripleWritePlan,
) -> Result<TerminalTailRunOutcome, TerminalTailWriteError>
where
    S: TapeIndexReplicaRecordSource + ?Sized,
    A: TerminalTailAuthority + ?Sized,
{
    for _ in 0..TERMINAL_TAIL_COMPONENT_COUNT {
        match write_terminal_tail_step(sink, source, authority, plan)? {
            TerminalTailStepOutcome::Advanced(_) => {}
            TerminalTailStepOutcome::AlreadyComplete => {
                return Ok(TerminalTailRunOutcome::Complete);
            }
            TerminalTailStepOutcome::RecoveryRequired {
                progress,
                component,
                evidence,
            } => {
                return Ok(TerminalTailRunOutcome::RecoveryRequired {
                    progress,
                    component,
                    evidence,
                });
            }
        }
    }

    match write_terminal_tail_step(sink, source, authority, plan)? {
        TerminalTailStepOutcome::AlreadyComplete => Ok(TerminalTailRunOutcome::Complete),
        TerminalTailStepOutcome::RecoveryRequired {
            progress,
            component,
            evidence,
        } => Ok(TerminalTailRunOutcome::RecoveryRequired {
            progress,
            component,
            evidence,
        }),
        TerminalTailStepOutcome::Advanced(_) => Err(TerminalTailWriteError::PlanMismatch(
            "terminal writer advanced more than five components",
        )),
    }
}

/// Stream one restart-safe component using immutable plans and replayable rows.
pub fn write_terminal_tail_step<S, A>(
    sink: &mut dyn RawTapeSink,
    source: &mut S,
    authority: &mut A,
    plan: &TerminalTripleWritePlan,
) -> Result<TerminalTailStepOutcome, TerminalTailWriteError>
where
    S: TapeIndexReplicaRecordSource + ?Sized,
    A: TerminalTailAuthority + ?Sized,
{
    plan.validate()?;
    let progress = authority
        .load_progress()
        .map_err(TerminalTailWriteError::Authority)?;
    let Some(component_index) = progress.next_component_index() else {
        validate_position(
            sink.position()?,
            plan.edition.descriptor.terminal_layout.partition,
            plan.edition.descriptor.terminal_layout.expected_eod_lba,
        )?;
        return Ok(TerminalTailStepOutcome::AlreadyComplete);
    };
    debug_assert!(component_index < TERMINAL_TAIL_COMPONENT_COUNT);
    let component = plan.component(component_index);
    let evidence = authority
        .reconcile_next(progress, component)
        .map_err(TerminalTailWriteError::Authority)?;
    let expected_after = component
        .planned_start_lba
        .checked_add(component.record_count)
        .and_then(|lba| lba.checked_add(1))
        .ok_or(TerminalTailLayoutError::ArithmeticOverflow {
            context: "terminal component post-filemark position",
        })?;
    let (reconciled_existing_component, rewrote_torn_component) = match evidence {
        TerminalComponentReconcileEvidence::Absent
        | TerminalComponentReconcileEvidence::TornRewritable => {
            let rewrote_torn_component =
                evidence == TerminalComponentReconcileEvidence::TornRewritable;
            if rewrote_torn_component {
                sink.locate_for_overwrite(PhysicalPositionHint {
                    partition: plan.edition.descriptor.terminal_layout.partition,
                    lba: component.planned_start_lba,
                })?;
            }
            validate_position(
                sink.position()?,
                plan.edition.descriptor.terminal_layout.partition,
                component.planned_start_lba,
            )?;
            emit_component(sink, source, plan, component_index)?;
            let filemark = sink.write_filemarks(1, true)?;
            validate_write_outcome(filemark, None)?;
            validate_position(
                filemark.position_after(),
                plan.edition.descriptor.terminal_layout.partition,
                expected_after,
            )?;
            (false, rewrote_torn_component)
        }
        TerminalComponentReconcileEvidence::Complete => {
            validate_position(
                sink.position()?,
                plan.edition.descriptor.terminal_layout.partition,
                expected_after,
            )?;
            (true, false)
        }
        TerminalComponentReconcileEvidence::TornWorm
        | TerminalComponentReconcileEvidence::Unproved => {
            return Ok(TerminalTailStepOutcome::RecoveryRequired {
                progress,
                component,
                evidence,
            });
        }
    };

    let barrier = sink.write_filemarks(0, false)?;
    validate_write_outcome(barrier, None)?;
    validate_position(
        barrier.position_after(),
        plan.edition.descriptor.terminal_layout.partition,
        expected_after,
    )?;
    let observed = sink.position()?;
    validate_position(
        observed,
        plan.edition.descriptor.terminal_layout.partition,
        expected_after,
    )?;
    let next_progress = progress
        .successor()
        .ok_or(TerminalTailWriteError::PlanMismatch(
            "complete progress unexpectedly has a next component",
        ))?;
    let journal_bundle = terminal_component_bundle(plan, component)?;
    let checkpoint_bundle = CommittedBundle {
        kind: CommittedBundleKind::CheckpointedThrough,
        entries: Vec::new(),
        highest_protected_ordinal: journal_bundle.highest_protected_ordinal,
        total_committed_ordinals: journal_bundle.total_committed_ordinals,
    };
    let commit = TerminalComponentCommit {
        previous_progress: progress,
        next_progress,
        component,
        observed_position: observed,
        reconciled_existing_component,
        rewrote_torn_component,
        journal_bundle,
        checkpoint_bundle,
    };
    authority
        .commit_after_barrier(&commit)
        .map_err(TerminalTailWriteError::Authority)?;
    let durable = authority
        .load_progress()
        .map_err(TerminalTailWriteError::Authority)?;
    if durable != next_progress {
        return Err(TerminalTailWriteError::Authority(format!(
            "component callback returned before progress was durable: observed {durable:?}, expected {next_progress:?}"
        )));
    }
    Ok(TerminalTailStepOutcome::Advanced(commit))
}

fn emit_component<S: TapeIndexReplicaRecordSource + ?Sized>(
    sink: &mut dyn RawTapeSink,
    source: &mut S,
    plan: &TerminalTripleWritePlan,
    component_index: usize,
) -> Result<(), TerminalTailWriteError> {
    let component = plan.component(component_index);
    match component.kind {
        TerminalTailComponentKind::TapeIndexReplica => {
            let replica_index = usize::from(component.ordinal - 1);
            let replica = &plan.replicas[replica_index];
            let observation = TapeIndexReplicaObservation {
                tape_file_number: component.planned_tape_file_number,
                start_lba: component.planned_start_lba,
                record_count: component.record_count,
            };
            let mut media_error = None;
            let result =
                write_tape_index_replica(
                    replica,
                    observation,
                    source,
                    |block| match write_one_block(sink, block) {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            media_error = Some(error);
                            Err(ParityError::Invariant("terminal replica raw write failed"))
                        }
                    },
                );
            if let Some(error) = media_error {
                return Err(error);
            }
            result?;
        }
        TerminalTailComponentKind::IndexSeparationExtent => {
            let gap_index = usize::from(component.ordinal - 1);
            let separation = &plan.separations[gap_index];
            let observation = IndexSeparationObservation {
                tape_file_number: component.planned_tape_file_number,
                start_lba: component.planned_start_lba,
                record_count: component.record_count,
            };
            let mut media_error = None;
            let result = write_index_separation(separation, observation, |block| {
                match write_one_block(sink, block) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        media_error = Some(error);
                        Err("terminal separation raw write failed".to_string())
                    }
                }
            });
            if let Some(error) = media_error {
                return Err(error);
            }
            result?;
        }
    }
    Ok(())
}

fn write_one_block(sink: &mut dyn RawTapeSink, block: &[u8]) -> Result<(), TerminalTailWriteError> {
    let expected = u32::try_from(block.len())
        .map_err(|_| TerminalTailWriteError::PlanMismatch("fixed block length does not fit u32"))?;
    let outcome = sink.write_fixed_block(block)?;
    validate_write_outcome(outcome, Some(expected))
}

fn validate_write_outcome(
    outcome: RawWriteOutcome,
    expected_bytes: Option<u32>,
) -> Result<(), TerminalTailWriteError> {
    if outcome.end_of_medium() {
        return Err(TerminalTailWriteError::EndOfMedium);
    }
    match (outcome, expected_bytes) {
        (RawWriteOutcome::WroteBlock { bytes_written, .. }, Some(expected))
            if bytes_written != expected =>
        {
            Err(TerminalTailWriteError::ShortWrite {
                expected,
                actual: bytes_written,
            })
        }
        (RawWriteOutcome::WroteBlock { .. }, Some(_))
        | (RawWriteOutcome::WroteFilemark { .. }, None) => Ok(()),
        _ => Err(TerminalTailWriteError::PlanMismatch(
            "raw sink returned the wrong completion kind",
        )),
    }
}

fn validate_position(
    actual: PhysicalPositionHint,
    expected_partition: u32,
    expected_lba: u64,
) -> Result<(), TerminalTailWriteError> {
    if actual.partition != expected_partition || actual.lba != expected_lba {
        return Err(TerminalTailWriteError::PositionMismatch {
            expected_partition,
            expected_lba,
            actual_partition: actual.partition,
            actual_lba: actual.lba,
        });
    }
    Ok(())
}

/// Build the canonical sink-journal bundle for one exact terminal component.
///
/// This is deterministic and safe to call during After-C restart projection
/// when no writer callback runs.
pub fn terminal_component_bundle(
    plan: &TerminalTripleWritePlan,
    component: TerminalTailComponentPlan,
) -> Result<CommittedBundle, TerminalTailWriteError> {
    plan.validate()?;
    if !plan
        .edition
        .descriptor
        .terminal_layout
        .components
        .contains(&component)
    {
        return Err(TerminalTailWriteError::PlanMismatch(
            "terminal component is outside the immutable triple plan",
        ));
    }
    let kind = match component.kind {
        TerminalTailComponentKind::TapeIndexReplica => TapeFileKind::TapeIndexReplica,
        TerminalTailComponentKind::IndexSeparationExtent => TapeFileKind::IndexSeparationExtent,
    };
    let canonical_metadata_hash = match component.kind {
        TerminalTailComponentKind::TapeIndexReplica => plan.edition.edition_digest,
        TerminalTailComponentKind::IndexSeparationExtent => {
            plan.separations[usize::from(component.ordinal - 1)].descriptor_digest
        }
    };
    Ok(CommittedBundle {
        kind: CommittedBundleKind::TerminalComponent,
        entries: vec![TapeFileEntry {
            tape_file_number: component.planned_tape_file_number,
            kind,
            block_count: component.record_count,
            physical_start_hint: Some(component.planned_start_lba),
            object_id: None,
            first_parity_data_ordinal: None,
            epoch_id: None,
            protected_ordinal_start: None,
            protected_ordinal_end_exclusive: None,
            canonical_metadata_hash: Some(canonical_metadata_hash),
            object_recovery_row: None,
        }],
        highest_protected_ordinal: plan.edition.descriptor.scope.highest_protected_ordinal,
        total_committed_ordinals: plan.edition.descriptor.scope.total_data_ordinals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    use remanence_library::{scsi::ScsiError, TapeIoError};

    use crate::raw::SpaceFilemarksOutcome;
    use crate::tape_index_replica::{
        checked_tape_index_replica_layout, plan_tape_index_edition, TapeIndexEditionDescriptor,
    };
    use crate::terminal_tail::TerminalTailLayout;
    use crate::{
        TapeIndexReplicaCounts, TapeIndexReplicaFileKind, TapeIndexReplicaMapEntry,
        TapeIndexReplicaObjectRow, TapeIndexReplicaRecordSource, TapeIndexReplicaScope,
    };

    const BLOCK_SIZE: u32 = 256 * 1024;

    #[derive(Clone)]
    struct MinimalSource;

    impl TapeIndexReplicaRecordSource for MinimalSource {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            visitor(&TapeIndexReplicaMapEntry {
                tape_file_number: 0,
                kind: TapeIndexReplicaFileKind::Bootstrap,
                block_count: 1,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            })
        }

        fn visit_object_rows(
            &mut self,
            _visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            Ok(())
        }
    }

    fn test_plan() -> TerminalTripleWritePlan {
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: 1,
            object_row_count: 0,
        };
        let replica_records = checked_tape_index_replica_layout(BLOCK_SIZE, counts)
            .expect("replica geometry")
            .replica_record_count;
        let layout = TerminalTailLayout::new(0, BLOCK_SIZE, 1, 2, replica_records, 3)
            .expect("terminal layout");
        let descriptor = TapeIndexEditionDescriptor {
            tape_uuid: [0x71; 16],
            edition_id: [0x72; 16],
            edition_sequence: 7,
            scope: TapeIndexReplicaScope {
                covered_prefix_tape_file_count: 1,
                total_data_ordinals: 0,
                highest_protected_ordinal: 0,
            },
            counts,
            block_size: BLOCK_SIZE,
            compression_enabled: false,
            writer_version: "terminal-writer-test".to_string(),
            write_timestamp: "2026-08-09T00:00:00Z".to_string(),
            terminal_layout: layout,
        };
        let mut source = MinimalSource;
        let edition = plan_tape_index_edition(descriptor, &mut source).expect("edition plan");
        let replicas = [
            plan_tape_index_replica(edition.clone(), 1).unwrap(),
            plan_tape_index_replica(edition.clone(), 2).unwrap(),
            plan_tape_index_replica(edition.clone(), 3).unwrap(),
        ];
        let gap = |ordinal| {
            plan_index_separation(IndexSeparationDescriptor {
                tape_uuid: edition.descriptor.tape_uuid,
                edition_id: edition.descriptor.edition_id,
                gap_ordinal: ordinal,
                block_size: BLOCK_SIZE,
                nominal_extent_bytes: u64::from(BLOCK_SIZE) * 3,
                total_records: 3,
                compression_enabled: false,
                terminal_layout: layout,
            })
            .unwrap()
        };
        let separations = [gap(1), gap(2)];
        TerminalTripleWritePlan::from_parts(edition, replicas, separations).unwrap()
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Fault {
        Block,
        Filemark,
        Barrier,
    }

    #[derive(Debug)]
    struct MemorySink {
        position: PhysicalPositionHint,
        fault: Option<Fault>,
        block_writes: usize,
        filemarks: usize,
        barriers: usize,
        overwrite_locates: usize,
        blocks: BTreeMap<u64, Vec<u8>>,
        filemark_lbas: BTreeSet<u64>,
    }

    #[derive(Clone, Copy, Debug)]
    enum ReadFaultKind {
        Transport,
        Medium,
        Hardware,
        BufferTooSmall,
    }

    struct FaultingRawSource<'a> {
        inner: &'a mut dyn RawTapeSource,
        cursor: PhysicalPositionHint,
        read_fault: Option<(u64, ReadFaultKind)>,
        locate_fault_lba: Option<u64>,
    }

    impl<'a> FaultingRawSource<'a> {
        fn new(inner: &'a mut dyn RawTapeSource) -> Self {
            let cursor = inner.position().expect("fixture position is readable");
            Self {
                inner,
                cursor,
                read_fault: None,
                locate_fault_lba: None,
            }
        }

        fn fail_read(mut self, lba: u64, kind: ReadFaultKind) -> Self {
            self.read_fault = Some((lba, kind));
            self
        }

        fn fail_locate(mut self, lba: u64) -> Self {
            self.locate_fault_lba = Some(lba);
            self
        }
    }

    fn injected_read_error(kind: ReadFaultKind) -> ParityError {
        let error = match kind {
            ReadFaultKind::Transport => TapeIoError::Transport(ScsiError::TransportError {
                status: 0,
                host_status: 0,
                driver_status: 0x06,
                info: 1,
                sense: Vec::new(),
            }),
            ReadFaultKind::Medium | ReadFaultKind::Hardware => {
                let mut sense = vec![0u8; 18];
                sense[0] = 0x70;
                sense[2] = match kind {
                    ReadFaultKind::Medium => 0x03,
                    ReadFaultKind::Hardware => 0x04,
                    _ => unreachable!(),
                };
                sense[7] = 10;
                sense[12] = 0x11;
                TapeIoError::CheckCondition(ScsiError::CheckCondition {
                    sense,
                    bytes_transferred: 0,
                })
            }
            ReadFaultKind::BufferTooSmall => TapeIoError::ReadBufferTooSmall {
                actual: BLOCK_SIZE.saturating_add(1),
                provided: BLOCK_SIZE,
            },
        };
        ParityError::TapeIo(error)
    }

    impl RawTapeSource for FaultingRawSource<'_> {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            self.inner.configure_fixed_block_size(block_size)
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            if self.locate_fault_lba == Some(hint.lba) {
                return Err(ParityError::TapeIo(TapeIoError::OperationFailed(
                    "injected locate failure".to_string(),
                )));
            }
            self.inner.locate_physical(hint)?;
            self.cursor = hint;
            Ok(())
        }

        fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            let position = self.inner.locate_end_of_data()?;
            self.cursor = position;
            Ok(position)
        }

        fn space_filemarks(&mut self, count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            let outcome = self.inner.space_filemarks(count)?;
            self.cursor = outcome.position_after;
            Ok(outcome)
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            if let Some((lba, kind)) = self.read_fault {
                if self.cursor.lba == lba {
                    return Err(injected_read_error(kind));
                }
            }
            let outcome = self.inner.read_record(buf)?;
            self.cursor = outcome.position_after();
            Ok(outcome)
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            let position = self.inner.position()?;
            self.cursor = position;
            Ok(position)
        }
    }

    impl MemorySink {
        fn new(lba: u64) -> Self {
            Self {
                position: PhysicalPositionHint::new(lba),
                fault: None,
                block_writes: 0,
                filemarks: 0,
                barriers: 0,
                overwrite_locates: 0,
                blocks: BTreeMap::new(),
                filemark_lbas: BTreeSet::new(),
            }
        }

        fn fail(&mut self, fault: Fault) {
            self.fault = Some(fault);
        }

        fn maybe_fail(&mut self, fault: Fault) -> Result<(), ParityError> {
            if self.fault == Some(fault) {
                self.fault = None;
                return Err(ParityError::Invariant("injected terminal writer fault"));
            }
            Ok(())
        }
    }

    impl RawTapeSink for MemorySink {
        fn locate_for_overwrite(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.position = hint;
            self.overwrite_locates += 1;
            self.blocks.retain(|lba, _| *lba < hint.lba);
            self.filemark_lbas.retain(|lba| *lba < hint.lba);
            Ok(())
        }

        fn write_fixed_block(&mut self, buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
            self.maybe_fail(Fault::Block)?;
            self.block_writes += 1;
            self.blocks.insert(self.position.lba, buf.to_vec());
            self.position.lba += 1;
            Ok(RawWriteOutcome::WroteBlock {
                bytes_written: buf.len() as u32,
                position_after: self.position,
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemarks(
            &mut self,
            count: u32,
            _immed: bool,
        ) -> Result<RawWriteOutcome, ParityError> {
            if count == 0 {
                self.maybe_fail(Fault::Barrier)?;
                self.barriers += 1;
            } else {
                self.maybe_fail(Fault::Filemark)?;
                self.filemarks += usize::try_from(count).unwrap();
                for _ in 0..count {
                    self.filemark_lbas.insert(self.position.lba);
                    self.position.lba += 1;
                }
            }
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: self.position,
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(self.position)
        }
    }

    impl RawTapeSource for MemorySink {
        fn configure_fixed_block_size(&mut self, _block_size: u32) -> Result<(), ParityError> {
            Ok(())
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            self.position = hint;
            Ok(())
        }

        fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            let last_block = self.blocks.keys().next_back().copied();
            let last_filemark = self.filemark_lbas.iter().next_back().copied();
            self.position.lba = last_block
                .into_iter()
                .chain(last_filemark)
                .max()
                .map_or(0, |lba| lba.saturating_add(1));
            Ok(self.position)
        }

        fn space_filemarks(&mut self, _count: i64) -> Result<SpaceFilemarksOutcome, ParityError> {
            Err(ParityError::Invariant(
                "terminal reconciliation fixture does not space filemarks",
            ))
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            if let Some(block) = self.blocks.get(&self.position.lba) {
                buf[..block.len()].copy_from_slice(block);
                self.position.lba += 1;
                return Ok(RawReadOutcome::Block {
                    bytes: block.len(),
                    position_after: self.position,
                });
            }
            if self.filemark_lbas.contains(&self.position.lba) {
                self.position.lba += 1;
                return Ok(RawReadOutcome::Filemark {
                    position_after: self.position,
                });
            }
            Ok(RawReadOutcome::EndOfData {
                position_after: self.position,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(self.position)
        }
    }

    #[derive(Debug)]
    struct MemoryAuthority {
        progress: TerminalTailProgress,
        evidence: TerminalComponentReconcileEvidence,
        fail_commit_once: bool,
        commits: Vec<TerminalComponentCommit>,
    }

    impl Default for MemoryAuthority {
        fn default() -> Self {
            Self {
                progress: TerminalTailProgress::BeforeReplicaA,
                evidence: TerminalComponentReconcileEvidence::Absent,
                fail_commit_once: false,
                commits: Vec::new(),
            }
        }
    }

    impl TerminalTailAuthority for MemoryAuthority {
        fn load_progress(&mut self) -> Result<TerminalTailProgress, String> {
            Ok(self.progress)
        }

        fn reconcile_next(
            &mut self,
            progress: TerminalTailProgress,
            _component: TerminalTailComponentPlan,
        ) -> Result<TerminalComponentReconcileEvidence, String> {
            if progress != self.progress {
                return Err("stale progress".to_string());
            }
            Ok(self.evidence)
        }

        fn commit_after_barrier(&mut self, commit: &TerminalComponentCommit) -> Result<(), String> {
            if self.fail_commit_once {
                self.fail_commit_once = false;
                return Err("injected durable authority failure".to_string());
            }
            if commit.previous_progress != self.progress
                || commit.next_progress != self.progress.successor().unwrap()
            {
                return Err("non-monotonic component commit".to_string());
            }
            self.progress = commit.next_progress;
            self.evidence = TerminalComponentReconcileEvidence::Absent;
            self.commits.push(commit.clone());
            Ok(())
        }
    }

    fn advance_one(
        sink: &mut MemorySink,
        authority: &mut MemoryAuthority,
        plan: &TerminalTripleWritePlan,
    ) -> TerminalComponentCommit {
        let mut source = MinimalSource;
        match write_terminal_tail_step(sink, &mut source, authority, plan).unwrap() {
            TerminalTailStepOutcome::Advanced(commit) => commit,
            other => panic!("expected advance, got {other:?}"),
        }
    }

    #[test]
    fn streams_exact_a_gap_b_gap_c_and_never_appends_a_second_triple() {
        let plan = test_plan();
        let mut sink = MemorySink::new(2);
        let mut authority = MemoryAuthority::default();
        for expected_index in 0..TERMINAL_TAIL_COMPONENT_COUNT {
            let commit = advance_one(&mut sink, &mut authority, &plan);
            assert_eq!(commit.component, plan.component(expected_index));
            assert_eq!(
                commit.journal_bundle.kind,
                CommittedBundleKind::TerminalComponent
            );
            assert_eq!(
                commit.journal_bundle,
                terminal_component_bundle(&plan, commit.component).unwrap()
            );
            assert_eq!(
                commit.checkpoint_bundle.kind,
                CommittedBundleKind::CheckpointedThrough
            );
        }
        assert_eq!(authority.progress, TerminalTailProgress::AfterReplicaC);
        let counts = (sink.block_writes, sink.filemarks, sink.barriers);
        let mut source = MinimalSource;
        assert_eq!(
            write_terminal_tail_step(&mut sink, &mut source, &mut authority, &plan).unwrap(),
            TerminalTailStepOutcome::AlreadyComplete
        );
        assert_eq!((sink.block_writes, sink.filemarks, sink.barriers), counts);
        assert_eq!(authority.commits.len(), 5);
        assert_eq!(authority.progress.completed_replicas(), 3);
    }

    #[test]
    fn run_to_completion_uses_the_same_five_durable_steps() {
        let plan = test_plan();
        let mut sink = MemorySink::new(2);
        let mut authority = MemoryAuthority::default();
        let mut source = MinimalSource;

        assert_eq!(
            write_terminal_tail(&mut sink, &mut source, &mut authority, &plan).unwrap(),
            TerminalTailRunOutcome::Complete
        );
        assert_eq!(authority.progress, TerminalTailProgress::AfterReplicaC);
        assert_eq!(authority.commits.len(), TERMINAL_TAIL_COMPONENT_COUNT);
        assert_eq!(sink.filemarks, TERMINAL_TAIL_COMPONENT_COUNT);
        assert_eq!(sink.barriers, TERMINAL_TAIL_COMPONENT_COUNT);

        let counts = (sink.block_writes, sink.filemarks, sink.barriers);
        assert_eq!(
            write_terminal_tail(&mut sink, &mut source, &mut authority, &plan).unwrap(),
            TerminalTailRunOutcome::Complete
        );
        assert_eq!((sink.block_writes, sink.filemarks, sink.barriers), counts);
    }

    #[test]
    fn barrier_or_authority_crash_reuses_complete_component_without_rewrite() {
        for fail_authority in [false, true] {
            for component_index in 0..TERMINAL_TAIL_COMPONENT_COUNT {
                let plan = test_plan();
                let mut sink = MemorySink::new(2);
                let mut authority = MemoryAuthority::default();
                for _ in 0..component_index {
                    advance_one(&mut sink, &mut authority, &plan);
                }
                if fail_authority {
                    authority.fail_commit_once = true;
                } else {
                    sink.fail(Fault::Barrier);
                }
                let mut source = MinimalSource;
                assert!(
                    write_terminal_tail_step(&mut sink, &mut source, &mut authority, &plan)
                        .is_err()
                );
                let media_counts = (sink.block_writes, sink.filemarks);
                authority.evidence = TerminalComponentReconcileEvidence::Complete;
                let commit = advance_one(&mut sink, &mut authority, &plan);
                assert!(commit.reconciled_existing_component);
                assert_eq!((sink.block_writes, sink.filemarks), media_counts);
            }
        }
    }

    #[test]
    fn proved_rewritable_torn_component_and_later_tail_are_rewritten_in_place() {
        for component_index in 0..TERMINAL_TAIL_COMPONENT_COUNT {
            let plan = test_plan();
            let mut sink = MemorySink::new(2);
            let mut authority = MemoryAuthority::default();
            for _ in 0..component_index {
                advance_one(&mut sink, &mut authority, &plan);
            }
            let before = authority.progress;
            sink.fail(Fault::Filemark);
            let mut source = MinimalSource;
            assert!(
                write_terminal_tail_step(&mut sink, &mut source, &mut authority, &plan).is_err()
            );
            assert_eq!(authority.progress, before);
            authority.evidence = TerminalComponentReconcileEvidence::TornRewritable;
            let mut source = MinimalSource;
            assert_eq!(
                write_terminal_tail(&mut sink, &mut source, &mut authority, &plan).unwrap(),
                TerminalTailRunOutcome::Complete
            );
            assert_eq!(authority.progress, TerminalTailProgress::AfterReplicaC);
            assert_eq!(sink.overwrite_locates, 1);
            let repaired = &authority.commits[component_index];
            assert!(repaired.rewrote_torn_component);
            assert!(!repaired.reconciled_existing_component);
            assert!(authority.commits[component_index + 1..]
                .iter()
                .all(|commit| !commit.rewrote_torn_component));
        }
    }

    #[test]
    fn worm_or_unproved_torn_component_never_advances_or_writes() {
        for evidence in [
            TerminalComponentReconcileEvidence::TornWorm,
            TerminalComponentReconcileEvidence::Unproved,
        ] {
            for component_index in 0..TERMINAL_TAIL_COMPONENT_COUNT {
                let plan = test_plan();
                let mut sink = MemorySink::new(2);
                let mut authority = MemoryAuthority::default();
                for _ in 0..component_index {
                    advance_one(&mut sink, &mut authority, &plan);
                }
                let progress = authority.progress;
                let counts = (
                    sink.block_writes,
                    sink.filemarks,
                    sink.barriers,
                    sink.overwrite_locates,
                );
                authority.evidence = evidence;
                let mut source = MinimalSource;
                let outcome =
                    write_terminal_tail_step(&mut sink, &mut source, &mut authority, &plan)
                        .expect("unrepairable tail classification is a durable recovery outcome");
                assert_eq!(
                    outcome,
                    TerminalTailStepOutcome::RecoveryRequired {
                        progress,
                        component: plan.component(component_index),
                        evidence,
                    }
                );
                assert_eq!(authority.progress, progress);
                assert_eq!(
                    (
                        sink.block_writes,
                        sink.filemarks,
                        sink.barriers,
                        sink.overwrite_locates,
                    ),
                    counts,
                    "unrepairable evidence must cause no media motion"
                );
            }
        }
    }

    #[test]
    fn first_block_failure_is_absent_and_restart_writes_once() {
        let plan = test_plan();
        let mut sink = MemorySink::new(2);
        let mut authority = MemoryAuthority::default();
        sink.fail(Fault::Block);
        let mut source = MinimalSource;
        assert!(write_terminal_tail_step(&mut sink, &mut source, &mut authority, &plan).is_err());
        assert_eq!(authority.progress, TerminalTailProgress::BeforeReplicaA);
        assert_eq!(sink.position.lba, 2);
        let commit = advance_one(&mut sink, &mut authority, &plan);
        assert!(!commit.reconciled_existing_component);
    }

    #[test]
    fn bounded_reconciler_validates_body_footer_and_filemark() {
        let plan = test_plan();
        let mut sink = MemorySink::new(2);
        let mut authority = MemoryAuthority::default();
        let mut source = MinimalSource;
        write_terminal_tail(&mut sink, &mut source, &mut authority, &plan).unwrap();

        assert_eq!(
            reconcile_terminal_tail_next(
                &mut sink,
                &plan,
                TerminalTailProgress::AfterReplicaA,
                true,
            ),
            TerminalComponentReconcileEvidence::Complete
        );

        let gap = plan.component(1);
        sink.blocks
            .get_mut(&gap.planned_start_lba)
            .expect("gap header exists")[0] ^= 0x80;
        assert_eq!(
            reconcile_terminal_tail_next(
                &mut sink,
                &plan,
                TerminalTailProgress::AfterReplicaA,
                true,
            ),
            TerminalComponentReconcileEvidence::TornRewritable
        );
        assert_eq!(sink.position.lba, gap.planned_start_lba);
        assert_eq!(
            reconcile_terminal_tail_next(
                &mut sink,
                &plan,
                TerminalTailProgress::AfterReplicaA,
                false,
            ),
            TerminalComponentReconcileEvidence::TornWorm
        );
    }

    #[test]
    fn read_failures_only_authorize_rewrite_when_the_record_is_proved_damaged() {
        let plan = test_plan();
        let mut sink = MemorySink::new(2);
        let mut authority = MemoryAuthority::default();
        let mut records = MinimalSource;
        write_terminal_tail(&mut sink, &mut records, &mut authority, &plan)
            .expect("complete terminal tail fixture writes");

        for component_index in [0usize, 1] {
            let progress = match component_index {
                0 => TerminalTailProgress::BeforeReplicaA,
                1 => TerminalTailProgress::AfterReplicaA,
                _ => unreachable!(),
            };
            let component = plan.component(component_index);
            let read_lbas = [
                component.planned_start_lba,
                component.planned_start_lba + 1,
                component.planned_start_lba + component.record_count - 1,
                component.planned_start_lba + component.record_count,
            ];
            for lba in read_lbas {
                for kind in [ReadFaultKind::Transport, ReadFaultKind::Hardware] {
                    let mut source = FaultingRawSource::new(&mut sink).fail_read(lba, kind);
                    assert_eq!(
                        reconcile_terminal_tail_next(&mut source, &plan, progress, true),
                        TerminalComponentReconcileEvidence::Unproved,
                        "non-medium {kind:?} failure at component {component_index} lba {lba} must not authorize overwrite"
                    );
                }
                for kind in [ReadFaultKind::Medium, ReadFaultKind::BufferTooSmall] {
                    let mut source = FaultingRawSource::new(&mut sink).fail_read(lba, kind);
                    assert_eq!(
                        reconcile_terminal_tail_next(&mut source, &plan, progress, true),
                        TerminalComponentReconcileEvidence::TornRewritable,
                        "proved damage {kind:?} at component {component_index} lba {lba} should remain repairable"
                    );
                    let mut source = FaultingRawSource::new(&mut sink).fail_read(lba, kind);
                    assert_eq!(
                        reconcile_terminal_tail_next(&mut source, &plan, progress, false),
                        TerminalComponentReconcileEvidence::TornWorm,
                    );
                }
            }
        }
    }

    #[test]
    fn positioning_failures_never_become_torn_media_evidence() {
        let plan = test_plan();
        let mut sink = MemorySink::new(2);
        let mut authority = MemoryAuthority::default();
        let mut records = MinimalSource;
        write_terminal_tail(&mut sink, &mut records, &mut authority, &plan)
            .expect("complete terminal tail fixture writes");
        let replica = plan.component(0);
        for lba in [
            replica.planned_start_lba,
            replica.planned_start_lba + 1,
            replica.planned_start_lba + replica.record_count - 1,
            replica.planned_start_lba + replica.record_count,
        ] {
            let mut source = FaultingRawSource::new(&mut sink).fail_locate(lba);
            assert_eq!(
                reconcile_terminal_tail_next(
                    &mut source,
                    &plan,
                    TerminalTailProgress::BeforeReplicaA,
                    true,
                ),
                TerminalComponentReconcileEvidence::Unproved,
                "failed LOCATE to lba {lba} says nothing about record damage"
            );
        }
    }

    #[test]
    fn transport_failed_reconciliation_cannot_move_or_overwrite_media() {
        let plan = test_plan();
        let mut sink = MemorySink::new(2);
        let mut completed = MemoryAuthority::default();
        let mut records = MinimalSource;
        write_terminal_tail(&mut sink, &mut records, &mut completed, &plan)
            .expect("complete terminal tail fixture writes");
        let component = plan.component(0);
        let evidence = {
            let mut source = FaultingRawSource::new(&mut sink)
                .fail_read(component.planned_start_lba, ReadFaultKind::Transport);
            reconcile_terminal_tail_next(
                &mut source,
                &plan,
                TerminalTailProgress::BeforeReplicaA,
                true,
            )
        };
        assert_eq!(evidence, TerminalComponentReconcileEvidence::Unproved);

        let mut authority = MemoryAuthority {
            evidence,
            ..MemoryAuthority::default()
        };
        let counts = (
            sink.block_writes,
            sink.filemarks,
            sink.barriers,
            sink.overwrite_locates,
        );
        let mut records = MinimalSource;
        assert_eq!(
            write_terminal_tail_step(&mut sink, &mut records, &mut authority, &plan)
                .expect("unproved media returns a typed recovery outcome"),
            TerminalTailStepOutcome::RecoveryRequired {
                progress: TerminalTailProgress::BeforeReplicaA,
                component,
                evidence: TerminalComponentReconcileEvidence::Unproved,
            }
        );
        assert_eq!(
            (
                sink.block_writes,
                sink.filemarks,
                sink.barriers,
                sink.overwrite_locates,
            ),
            counts,
            "completion-unknown reconciliation must cause no media motion"
        );
        assert_eq!(authority.progress, TerminalTailProgress::BeforeReplicaA);
        assert!(authority.commits.is_empty());
    }

    #[test]
    fn component_footer_arithmetic_rejects_zero_count_and_lba_overflow() {
        assert_eq!(checked_component_footer_lba(7, 0), None);
        assert_eq!(checked_component_footer_lba(u64::MAX, 2), None);
        assert_eq!(checked_component_footer_lba(u64::MAX, 1), Some(u64::MAX));
    }

    #[test]
    fn position_mismatch_message_is_not_duplicated() {
        let error = TerminalTailWriteError::PositionMismatch {
            expected_partition: 0,
            expected_lba: 7,
            actual_partition: 0,
            actual_lba: 8,
        };
        assert_eq!(
            error.to_string(),
            "terminal writer observed partition 0 lba 8, expected partition 0 lba 7"
        );
    }
}
