//! Bounded fast inventory from the terminal triple-index tail.
//!
//! The reader starts with `SPACE(EOD)`, inspects at most the five terminal
//! tape-file boundaries, and then uses a validated replica layout for exact
//! absolute reads. It never classifies or walks Object files. Replica payloads
//! are decoded through the existing fixed-slot streaming codec.

use crate::bot_recovery::{
    recover_terminal_inventory_from_bot, recover_terminal_inventory_from_bot_with_authority,
    BotObjectRecoveryAuthority, BotStructuralRecoveryError, BotStructuralRecoverySummary,
};
#[cfg(test)]
use crate::bot_recovery::{reject_readable_foreign_bot_bootstrap, BotRecoveredObjectState};
use crate::error::ParityError;
use crate::filemark_map::{TapeFileKind, TapeFileMapEntry};
use crate::index_separation::{
    parse_index_separation_footer, parse_index_separation_header, validate_index_separation_full,
    validate_index_separation_pair, IndexSeparationError, IndexSeparationInteriorBlockSource,
};
use crate::raw::{
    tape_error_is_current_medium_damage, PhysicalPositionHint, RawReadOutcome, RawTapeSource,
};
use crate::scan::{scan_reconstruct_filemark_map_with_report, ScanDamageKind};
use crate::tape_index_replica::{
    parse_tape_index_bootstrap_footer, parse_tape_index_replica_header,
    validate_tape_index_replica_pair, validate_tape_index_replica_payload, TapeIndexEditionPlan,
    TapeIndexReplicaError, TapeIndexReplicaFileKind, TapeIndexReplicaHeader,
    TapeIndexReplicaMapEntry, TapeIndexReplicaObjectRow, TapeIndexReplicaPayloadBlockSource,
    TapeIndexReplicaPayloadSummary,
};
use crate::terminal_tail::{
    validate_terminal_index_block_size_hint, TerminalTailLayout, TERMINAL_INDEX_REPLICA_COUNT,
    TERMINAL_TAIL_COMPONENT_COUNT,
};
#[cfg(test)]
use remanence_library::TapeIoError;
use std::cell::RefCell;

/// Typed stage at which one terminal member failed validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalReplicaFailureKind {
    /// No complete member could be located at its planned position.
    Missing,
    /// The complete header record could not be read.
    HeaderRead,
    /// The header frame was malformed or belonged to another member.
    HeaderInvalid,
    /// The complete footer record could not be read.
    FooterRead,
    /// The footer frame or measured footer position was invalid.
    FooterInvalid,
    /// Header hash, local descriptor, or local observations disagreed.
    LocalBinding,
    /// A trailing filemark was absent or unreadable.
    TrailingFilemark,
    /// Fixed-slot payload streaming or digest validation failed.
    PayloadInvalid,
    /// An independently valid member disagreed with the selected survivor.
    CrossSurvivorConflict,
}

/// Typed degraded evidence for an unusable A/B/C member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalReplicaFailure {
    /// Stable failure category.
    pub kind: TerminalReplicaFailureKind,
    /// Diagnostic detail from the parser or physical source.
    pub detail: String,
}

/// Validation evidence for one one-based A/B/C ordinal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalReplicaEvidence {
    /// Header, payload, footer, placement, and filemark all validated.
    Valid {
        /// Streamed payload summary.
        summary: TapeIndexReplicaPayloadSummary,
    },
    /// Header/footer/local placement agree with the selected edition. Fast
    /// inventory deliberately did not stream this older member's payload.
    ConsistentEnvelope,
    /// This member cannot be used as inventory authority.
    Invalid(TerminalReplicaFailure),
}

/// Successful fast inventory selection and redundancy evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalInventorySelection {
    /// Monotonic stream attempt that emitted the authoritative row set.
    pub selected_attempt_id: u64,
    /// Selected member, preferring C (3), then B (2), then A (1).
    pub selected_replica_ordinal: u16,
    /// Selected edition facts after cross-survivor comparison.
    pub edition: TapeIndexEditionPlan,
    /// Summary from the selected member's streamed body.
    pub payload: TapeIndexReplicaPayloadSummary,
    /// A, B, and C evidence in ordinal order.
    pub replicas: [TerminalReplicaEvidence; 3],
}

/// One bounded event emitted while selecting C, then B, then A.
///
/// Rows are provisional until the returned [`TerminalInventorySelection`]
/// names their `attempt_id`. A failed candidate is explicitly rejected, so a
/// streaming consumer can discard its rows without retaining another member
/// or mistaking partial output for inventory authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalInventoryStreamEvent {
    /// A locally bound member body is about to be streamed and validated.
    ReplicaAttemptStarted {
        /// Monotonic identifier unique within this inventory call.
        attempt_id: u64,
        /// One-based A/B/C member ordinal.
        replica_ordinal: u16,
    },
    /// One canonical pre-tail structural row from a candidate member.
    StructuralEntry {
        /// Candidate attempt that owns this row.
        attempt_id: u64,
        /// One-based A/B/C member ordinal.
        replica_ordinal: u16,
        /// Decoded fixed-slot row.
        entry: TapeIndexReplicaMapEntry,
    },
    /// One Object recovery row from a candidate member.
    ObjectRow {
        /// Candidate attempt that owns this row.
        attempt_id: u64,
        /// One-based A/B/C member ordinal.
        replica_ordinal: u16,
        /// Decoded fixed-slot row.
        row: TapeIndexReplicaObjectRow,
    },
    /// A candidate failed validation after zero or more provisional rows.
    ReplicaAttemptRejected {
        /// Candidate attempt whose rows are not authoritative.
        attempt_id: u64,
        /// One-based A/B/C member ordinal.
        replica_ordinal: u16,
        /// Typed validation evidence retained for fallback reporting.
        failure: TerminalReplicaFailure,
    },
}

impl TerminalInventorySelection {
    /// True unless all three members independently validated and agreed.
    pub fn is_degraded(&self) -> bool {
        self.replicas
            .iter()
            .any(|evidence| matches!(evidence, TerminalReplicaEvidence::Invalid(_)))
    }
}

/// Why fast inventory must hand off to the BOT structural recovery scanner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BotStructuralRecoveryReason {
    /// No trustworthy terminal layout could be recovered from bounded tail
    /// footer inspection.
    NoUsableTerminalLayout,
    /// A layout was found, but A, B, and C all failed local validation.
    AllMembersInvalid,
}

/// Evidence returned instead of treating a missing terminal index as empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BotStructuralRecoveryRequired {
    /// Stable recovery reason.
    pub reason: BotStructuralRecoveryReason,
    /// A, B, and C evidence in ordinal order.
    pub replicas: [TerminalReplicaEvidence; 3],
}

/// Terminal inventory either succeeded or explicitly requires BOT recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalInventoryOutcome {
    /// Complete inventory rows were emitted from the selected member.
    Inventory(Box<TerminalInventorySelection>),
    /// Fast authority is unavailable; invoke structural recovery from BOT.
    BotStructuralRecoveryRequired(Box<BotStructuralRecoveryRequired>),
}

/// Physical positioning or post-selection streaming failure.
#[derive(Debug, thiserror::Error)]
pub enum TerminalInventoryReadError {
    /// The supplied fixed-block profile is unsupported.
    #[error("terminal inventory block-size validation failed: {0}")]
    BlockSize(#[from] crate::terminal_tail::TerminalTailLayoutError),
    /// An EOD/backward positioning operation failed before a member could be
    /// classified as valid or invalid.
    #[error("terminal inventory source operation {operation} failed: {message}")]
    Source {
        /// Stable operation name.
        operation: &'static str,
        /// Underlying source detail.
        message: String,
    },
    /// A previously validated selected member changed or a caller visitor
    /// rejected a row during the authoritative emission pass.
    #[error("selected terminal replica {ordinal} failed during inventory emission: {source}")]
    SelectedReplica {
        /// One-based A/B/C ordinal.
        ordinal: u16,
        /// Codec or visitor failure.
        source: TapeIndexReplicaError,
    },
    /// The bounded output consumer stopped accepting inventory events.
    #[error("terminal inventory stream visitor failed: {message}")]
    StreamVisitor {
        /// Caller-provided failure detail.
        message: String,
    },
    /// Independently valid terminal payloads carry conflicting editions.
    #[error("terminal inventory found {count} independently valid conflicting replica editions")]
    TerminalIndexReplicaConflict {
        /// Number of distinct independently valid editions.
        count: usize,
    },
}

/// Physical proof returned only after the complete prefix and terminal tail
/// have been measured and validated.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TerminalIndexCompleteEvidence {
    /// Common edition facts independently recovered from all three replicas.
    pub edition: TapeIndexEditionPlan,
    /// Independently streamed payload result for A, B, and C.
    pub replicas: [TapeIndexReplicaPayloadSummary; 3],
    /// Validated zero-filled interior record counts for gaps AB and BC.
    pub separation_interior_records: [u64; 2],
    /// Physical EOD measured before the BOT walk.
    pub measured_eod: PhysicalPositionHint,
    /// Number of canonical pre-tail tape files compared row by row.
    pub verified_prefix_tape_file_count: u64,
    /// Number of fixed records in that verified physical prefix.
    pub verified_prefix_record_count: u64,
    /// Number of filemark-delimited files measured from BOT through C.
    pub measured_tape_file_count: u64,
}

/// Full physical evidence for one separation extent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalSeparationEvidence {
    /// Header, footer, every interior record, and trailing filemark validated.
    Valid {
        /// Number of zero-filled interior records read.
        interior_record_count: u64,
    },
    /// The extent was missing, torn, corrupt, or physically misplaced.
    Invalid {
        /// Stable diagnostic detail.
        detail: String,
    },
}

/// Evidence shared by complete and degraded full-verification outcomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalIndexVerification {
    /// Canonical edition selected only from a physically valid survivor.
    pub edition: TapeIndexEditionPlan,
    /// Payload summary of the newest physically valid canonical survivor.
    pub selected_payload: TapeIndexReplicaPayloadSummary,
    /// A, B, and C full-payload evidence in ordinal order.
    pub replicas: [TerminalReplicaEvidence; 3],
    /// AB and BC full-extent evidence in ordinal order.
    pub separations: [TerminalSeparationEvidence; 2],
    /// Physical EOD measured before the BOT walk.
    pub measured_eod: PhysicalPositionHint,
    /// Number of canonical pre-tail tape files compared row by row.
    pub verified_prefix_tape_file_count: u64,
    /// Number of fixed records in that verified physical prefix.
    pub verified_prefix_record_count: u64,
    /// Number of structurally complete tape files measured from BOT.
    pub measured_tape_file_count: u64,
}

/// Full verification could not establish a canonical physical prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalIndexRecoveryRequired {
    /// Physical EOD measured before the BOT recovery walk.
    pub measured_eod: PhysicalPositionHint,
    /// BOT structural recovery summary; never an empty-success placeholder.
    pub bot_recovery: BotStructuralRecoverySummary,
    /// A, B, and C evidence available before the recovery decision.
    pub replicas: [TerminalReplicaEvidence; 3],
    /// Stable recovery reason.
    pub detail: String,
}

/// Typed full physical verification outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalIndexVerificationOutcome {
    /// Canonical prefix, A/B/C, AB/BC, and terminal EOD all validated.
    VerifiedComplete(Box<TerminalIndexVerification>),
    /// Canonical prefix has a valid survivor, but redundant tail evidence is degraded.
    VerifiedDegraded(Box<TerminalIndexVerification>),
    /// No survivor could prove the physical prefix; BOT recovery evidence is attached.
    RecoveryRequired(Box<TerminalIndexRecoveryRequired>),
}

/// Typed reason a full physical terminal-index verification failed.
#[derive(Debug, thiserror::Error)]
pub enum TerminalIndexVerificationError {
    /// The supplied fixed-block profile is unsupported.
    #[error("terminal index verification block-size validation failed: {0}")]
    BlockSize(#[from] crate::terminal_tail::TerminalTailLayoutError),
    /// A positioning or record-read operation failed.
    #[error("terminal index verification source operation {operation} failed: {message}")]
    Source {
        /// Stable operation name.
        operation: &'static str,
        /// Underlying source detail.
        message: String,
    },
    /// No complete terminal layout was recoverable from EOD.
    #[error("no complete terminal layout ends at measured EOD {measured_eod_lba}")]
    NoCompleteLayout {
        /// Physical EOD returned by the source.
        measured_eod_lba: u64,
    },
    /// Surviving terminal footers proposed different complete layouts.
    #[error("conflicting complete terminal layouts were recovered from EOD ({count} candidates)")]
    ConflictingLayouts {
        /// Number of distinct layouts ending at measured EOD.
        count: usize,
    },
    /// Independently valid members in one layout named different editions.
    #[error("conflicting terminal replica editions were recovered ({count} candidates)")]
    ConflictingReplicaEditions {
        /// Number of independently valid, mutually conflicting editions.
        count: usize,
    },
    /// The BOT structural walk itself failed.
    #[error("physical prefix walk failed: {message}")]
    PrefixWalk {
        /// Scanner detail.
        message: String,
    },
    /// Separately durable Object recovery authority was invalid or conflicted
    /// with the measured physical prefix.
    #[error("BOT Object recovery authority failed: {message}")]
    RecoveryAuthority {
        /// Authority validation or conflict detail.
        message: String,
    },
    /// The physical walk found a torn final tape file.
    #[error("physical prefix walk found incomplete tape file {tape_file_number}: {kind:?}")]
    PrefixTruncated {
        /// Dense tape-file number at the truncation.
        tape_file_number: u64,
        /// Typed scanner truncation category.
        kind: crate::scan::ScanTailTruncationKind,
    },
    /// The physical walk encountered media damage or invalid structural framing.
    #[error("physical prefix walk reported {count} damaged region(s); first kind {first_kind:?}")]
    PrefixDamaged {
        /// Total damaged regions.
        count: usize,
        /// First typed damage category.
        first_kind: ScanDamageKind,
    },
    /// The measured file count disagreed with the complete five-file layout.
    #[error("measured tape-file count {actual}, expected {expected}")]
    TapeFileCountMismatch {
        /// Planned file count through replica C.
        expected: u64,
        /// Measured file count.
        actual: u64,
    },
    /// A measured terminal tape file disagreed with its five-component plan.
    #[error("measured terminal component at tape file {tape_file_number} mismatched: {detail}")]
    TerminalComponentMismatch {
        /// Dense tape-file number.
        tape_file_number: u64,
        /// Kind/count mismatch detail.
        detail: String,
    },
    /// A physical structural row disagreed with the canonical replica map.
    #[error("replica {ordinal} canonical map mismatch at tape file {tape_file_number}: {detail}")]
    CanonicalMapMismatch {
        /// One-based replica ordinal.
        ordinal: u16,
        /// Dense tape-file number under comparison.
        tape_file_number: u64,
        /// Stable mismatch detail.
        detail: String,
    },
    /// A complete replica failed local or payload validation.
    #[error("terminal replica {ordinal} failed full verification: {failure:?}: {detail}")]
    Replica {
        /// One-based replica ordinal.
        ordinal: u16,
        /// Stable validation phase.
        failure: TerminalReplicaFailureKind,
        /// Underlying detail.
        detail: String,
    },
    /// Independently valid replicas did not carry identical edition/payload facts.
    #[error("terminal replica {ordinal} conflicts with replica A")]
    CrossReplicaConflict {
        /// Conflicting one-based ordinal.
        ordinal: u16,
    },
    /// A typed separation extent failed framing, placement, or zero-fill checks.
    #[error("terminal separation {ordinal} failed full verification: {source}")]
    Separation {
        /// One-based gap ordinal (AB=1, BC=2).
        ordinal: u16,
        /// Typed codec or physical-source failure.
        #[source]
        source: IndexSeparationError,
    },
    /// Checked physical accounting overflowed.
    #[error("terminal index verification arithmetic overflow: {context}")]
    ArithmeticOverflow {
        /// Failed accounting operation.
        context: &'static str,
    },
}

struct LayoutInspection {
    replicas: [TerminalReplicaEvidence; 3],
    envelopes: [Option<ValidatedReplicaEnvelope>; 3],
}

enum TerminalMemberReadError {
    Invalid(TerminalReplicaFailure),
    Source(TerminalInventoryReadError),
}

#[derive(Clone)]
struct ValidatedReplicaEnvelope {
    header: TapeIndexReplicaHeader,
    footer: crate::tape_index_replica::TapeIndexBootstrapFooter,
}

/// Validate one healthy terminal-index body without walking the structural prefix.
///
/// The newest locally valid member is streamed exactly once. Older members are
/// inspected only through their bounded header/footer envelopes unless the
/// newer member fails payload validation. This is the production fast-inventory
/// path for callers that need the canonical counts and digests but do not need
/// each row delivered to a transactional catalog writer.
pub fn read_terminal_index_inventory_summary(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<TerminalInventoryOutcome, TerminalInventoryReadError> {
    read_terminal_index_inventory_streamed(source, tape_uuid, block_size, |_| Ok(()))
}

/// Stream candidate rows while selecting the newest valid terminal member.
///
/// On the ordinary C-to-B-to-A path, each attempted member body is read once.
/// Independently valid conflicting envelopes may require one bounded
/// pre-validation replay before authoritative emission. Events carry an
/// attempt id because rows precede the final payload digest: consumers must
/// commit only the attempt named by the returned selection and discard every
/// explicitly rejected attempt. This preserves bounded memory and propagates
/// downstream backpressure without weakening fallback.
pub fn read_terminal_index_inventory_streamed<F>(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    visit_event: F,
) -> Result<TerminalInventoryOutcome, TerminalInventoryReadError>
where
    F: FnMut(TerminalInventoryStreamEvent) -> Result<(), String>,
{
    let visit_event = RefCell::new(visit_event);
    validate_terminal_index_block_size_hint(block_size)?;
    source
        .configure_fixed_block_size(block_size)
        .map_err(|error| source_error("configure fixed block size", error))?;
    let eod = source
        .locate_end_of_data()
        .map_err(|error| source_error("SPACE(EOD)", error))?;
    if eod.partition != 0 {
        return Err(TerminalInventoryReadError::Source {
            operation: "SPACE(EOD)",
            message: format!("returned unsupported partition {}", eod.partition),
        });
    }

    let layouts = discover_terminal_layouts(source, tape_uuid, block_size, eod)?;
    if layouts.is_empty() {
        return Ok(TerminalInventoryOutcome::BotStructuralRecoveryRequired(
            Box::new(BotStructuralRecoveryRequired {
                reason: BotStructuralRecoveryReason::NoUsableTerminalLayout,
                replicas: missing_evidence("no valid terminal footer was found from EOD"),
            }),
        ));
    }

    let mut inspections = Vec::with_capacity(layouts.len());
    for layout in layouts {
        let inspection = inspect_layout(source, tape_uuid, block_size, layout)?;
        inspections.push((layout, inspection));
    }
    resolve_conflicting_survivor_envelopes(source, block_size, &mut inspections)?;

    let mut next_attempt_id = 1u64;
    for selected_index in (0..usize::from(TERMINAL_INDEX_REPLICA_COUNT)).rev() {
        for (_layout, inspection) in &mut inspections {
            let Some(envelope) = inspection.envelopes[selected_index].as_ref() else {
                continue;
            };
            let selected_ordinal = u16::try_from(selected_index + 1)
                .expect("terminal replica array has exactly three members");
            let attempt_id = next_attempt_id;
            next_attempt_id = next_attempt_id.checked_add(1).ok_or(
                TerminalInventoryReadError::StreamVisitor {
                    message: "terminal inventory attempt id overflow".to_string(),
                },
            )?;
            emit_inventory_event(
                &visit_event,
                TerminalInventoryStreamEvent::ReplicaAttemptStarted {
                    attempt_id,
                    replica_ordinal: selected_ordinal,
                },
            )?;

            let visitor_failure = RefCell::new(None);
            let mut visit_entry = |entry: &TapeIndexReplicaMapEntry| {
                let result =
                    visit_event.borrow_mut()(TerminalInventoryStreamEvent::StructuralEntry {
                        attempt_id,
                        replica_ordinal: selected_ordinal,
                        entry: entry.clone(),
                    });
                if let Err(message) = result {
                    *visitor_failure.borrow_mut() = Some(message.clone());
                    return Err(TapeIndexReplicaError::Payload { message });
                }
                Ok(())
            };
            let mut visit_row = |row: &TapeIndexReplicaObjectRow| {
                let result = visit_event.borrow_mut()(TerminalInventoryStreamEvent::ObjectRow {
                    attempt_id,
                    replica_ordinal: selected_ordinal,
                    row: row.clone(),
                });
                if let Err(message) = result {
                    *visitor_failure.borrow_mut() = Some(message.clone());
                    return Err(TapeIndexReplicaError::Payload { message });
                }
                Ok(())
            };
            let payload = validate_member_payload(
                source,
                block_size,
                envelope,
                &mut visit_entry,
                &mut visit_row,
            );
            if let Some(message) = visitor_failure.into_inner() {
                return Err(TerminalInventoryReadError::StreamVisitor { message });
            }
            let summary = match payload {
                Ok(summary) => summary,
                Err(TerminalMemberReadError::Invalid(failure)) => {
                    inspection.replicas[selected_index] =
                        TerminalReplicaEvidence::Invalid(failure.clone());
                    emit_inventory_event(
                        &visit_event,
                        TerminalInventoryStreamEvent::ReplicaAttemptRejected {
                            attempt_id,
                            replica_ordinal: selected_ordinal,
                            failure,
                        },
                    )?;
                    continue;
                }
                Err(TerminalMemberReadError::Source(error)) => return Err(error),
            };
            inspection.replicas[selected_index] = TerminalReplicaEvidence::Valid { summary };
            for index in 0..usize::from(TERMINAL_INDEX_REPLICA_COUNT) {
                if index == selected_index {
                    continue;
                }
                if let Some(candidate) = inspection.envelopes[index].as_ref() {
                    if matches!(
                        inspection.replicas[index],
                        TerminalReplicaEvidence::ConsistentEnvelope
                    ) && candidate.header.plan.edition != envelope.header.plan.edition
                    {
                        inspection.replicas[index] = TerminalReplicaEvidence::Invalid(
                            TerminalReplicaFailure {
                                kind: TerminalReplicaFailureKind::CrossSurvivorConflict,
                                detail: format!(
                                    "replica {} edition/scope/count/digest facts disagree with selected replica {}",
                                    index + 1,
                                    selected_index + 1
                                ),
                            },
                        );
                    }
                }
            }
            return Ok(TerminalInventoryOutcome::Inventory(Box::new(
                TerminalInventorySelection {
                    selected_attempt_id: attempt_id,
                    selected_replica_ordinal: selected_ordinal,
                    edition: envelope.header.plan.edition.clone(),
                    payload: summary,
                    replicas: inspection.replicas.clone(),
                },
            )));
        }
    }

    let replicas = inspections
        .into_iter()
        .next()
        .expect("nonempty terminal layout candidates produce an inspection")
        .1
        .replicas;
    Ok(TerminalInventoryOutcome::BotStructuralRecoveryRequired(
        Box::new(BotStructuralRecoveryRequired {
            reason: BotStructuralRecoveryReason::AllMembersInvalid,
            replicas,
        }),
    ))
}

fn resolve_conflicting_survivor_envelopes(
    source: &mut dyn RawTapeSource,
    block_size: u32,
    inspections: &mut [(TerminalTailLayout, LayoutInspection)],
) -> Result<(), TerminalInventoryReadError> {
    let mut envelope_editions = Vec::new();
    for (_, inspection) in inspections.iter() {
        for envelope in inspection.envelopes.iter().flatten() {
            if !envelope_editions.contains(&envelope.header.plan.edition) {
                envelope_editions.push(envelope.header.plan.edition.clone());
            }
        }
    }
    if envelope_editions.len() <= 1 {
        return Ok(());
    }

    let mut surviving_editions = Vec::new();
    for (_, inspection) in inspections.iter_mut() {
        for index in 0..usize::from(TERMINAL_INDEX_REPLICA_COUNT) {
            let Some(envelope) = inspection.envelopes[index].clone() else {
                continue;
            };
            match validate_member_payload(
                source,
                block_size,
                &envelope,
                &mut |_| Ok(()),
                &mut |_| Ok(()),
            ) {
                Ok(summary) => {
                    inspection.replicas[index] = TerminalReplicaEvidence::Valid { summary };
                    if !surviving_editions.contains(&envelope.header.plan.edition) {
                        surviving_editions.push(envelope.header.plan.edition.clone());
                    }
                }
                Err(TerminalMemberReadError::Invalid(failure)) => {
                    inspection.replicas[index] = TerminalReplicaEvidence::Invalid(failure);
                    inspection.envelopes[index] = None;
                }
                Err(TerminalMemberReadError::Source(error)) => return Err(error),
            }
        }
    }
    if surviving_editions.len() > 1 {
        return Err(TerminalInventoryReadError::TerminalIndexReplicaConflict {
            count: surviving_editions.len(),
        });
    }
    if let Some(survivor) = surviving_editions.first() {
        for (_, inspection) in inspections.iter_mut() {
            for index in 0..usize::from(TERMINAL_INDEX_REPLICA_COUNT) {
                if inspection.envelopes[index]
                    .as_ref()
                    .is_some_and(|envelope| envelope.header.plan.edition != *survivor)
                {
                    inspection.replicas[index] =
                        TerminalReplicaEvidence::Invalid(TerminalReplicaFailure {
                            kind: TerminalReplicaFailureKind::CrossSurvivorConflict,
                            detail:
                                "replica edition conflicts with the only payload-valid survivor"
                                    .to_string(),
                        });
                    inspection.envelopes[index] = None;
                }
            }
        }
    }
    Ok(())
}

fn emit_inventory_event<F>(
    visitor: &RefCell<F>,
    event: TerminalInventoryStreamEvent,
) -> Result<(), TerminalInventoryReadError>
where
    F: FnMut(TerminalInventoryStreamEvent) -> Result<(), String>,
{
    visitor.borrow_mut()(event)
        .map_err(|message| TerminalInventoryReadError::StreamVisitor { message })
}

/// Stream a validated terminal inventory into row visitors.
///
/// This compatibility surface first runs the bounded summary selection, then
/// replays the selected member into the supplied visitors. Callers must treat
/// visitor output as authoritative only when the function returns
/// [`TerminalInventoryOutcome::Inventory`]. Production summary-only inventory
/// should use [`read_terminal_index_inventory_summary`] so the selected body is
/// read exactly once.
pub fn read_terminal_index_inventory<FE, FR>(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    mut visit_entry: FE,
    mut visit_row: FR,
) -> Result<TerminalInventoryOutcome, TerminalInventoryReadError>
where
    FE: FnMut(&TapeIndexReplicaMapEntry) -> Result<(), TapeIndexReplicaError>,
    FR: FnMut(&TapeIndexReplicaObjectRow) -> Result<(), TapeIndexReplicaError>,
{
    let outcome = read_terminal_index_inventory_summary(source, tape_uuid, block_size)?;
    let TerminalInventoryOutcome::Inventory(mut selection) = outcome else {
        return Ok(outcome);
    };
    let selected_ordinal = selection.selected_replica_ordinal;
    let envelope = validate_member_envelope(
        source,
        tape_uuid,
        block_size,
        selection.edition.descriptor.terminal_layout,
        selected_ordinal,
    )
    .map_err(|error| selected_member_error(selected_ordinal, error))?;
    let replayed_summary = validate_member_payload(
        source,
        block_size,
        &envelope,
        &mut visit_entry,
        &mut visit_row,
    )
    .map_err(|error| selected_member_error(selected_ordinal, error))?;
    if replayed_summary != selection.payload {
        return Err(TerminalInventoryReadError::SelectedReplica {
            ordinal: selected_ordinal,
            source: TapeIndexReplicaError::DigestMismatch {
                field: "inventory replay summary",
            },
        });
    }
    selection.payload = replayed_summary;
    Ok(TerminalInventoryOutcome::Inventory(selection))
}

/// Perform a complete physical verification distinct from bounded inventory.
///
/// Integrity damage is a typed outcome, not a transport error: a verified
/// canonical prefix with damaged redundancy is `VerifiedDegraded`, while lack
/// of a surviving canonical authority returns `RecoveryRequired` with a real
/// BOT structural-recovery summary.
pub fn verify_terminal_index_full(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<TerminalIndexVerificationOutcome, TerminalIndexVerificationError> {
    verify_terminal_index_full_inner(source, tape_uuid, block_size, &mut None)
}

/// Perform complete physical verification with optional checkpoint identity
/// authority for the all-replicas-invalid BOT fallback.
pub fn verify_terminal_index_full_with_authority(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    authority: &mut dyn BotObjectRecoveryAuthority,
) -> Result<TerminalIndexVerificationOutcome, TerminalIndexVerificationError> {
    verify_terminal_index_full_inner(source, tape_uuid, block_size, &mut Some(authority))
}

fn verify_terminal_index_full_inner(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    authority: &mut Option<&mut dyn BotObjectRecoveryAuthority>,
) -> Result<TerminalIndexVerificationOutcome, TerminalIndexVerificationError> {
    match verify_terminal_index_strict(source, tape_uuid, block_size) {
        Ok(complete) => {
            let replicas = complete
                .replicas
                .map(|summary| TerminalReplicaEvidence::Valid { summary });
            let separations = complete
                .separation_interior_records
                .map(|interior_record_count| TerminalSeparationEvidence::Valid {
                    interior_record_count,
                });
            Ok(TerminalIndexVerificationOutcome::VerifiedComplete(
                Box::new(TerminalIndexVerification {
                    edition: complete.edition,
                    selected_payload: complete.replicas[2],
                    replicas,
                    separations,
                    measured_eod: complete.measured_eod,
                    verified_prefix_tape_file_count: complete.verified_prefix_tape_file_count,
                    verified_prefix_record_count: complete.verified_prefix_record_count,
                    measured_tape_file_count: complete.measured_tape_file_count,
                }),
            ))
        }
        Err(error) if verification_error_is_physical_damage(&error) => {
            verify_terminal_index_after_damage(
                source,
                tape_uuid,
                block_size,
                error.to_string(),
                authority,
            )
        }
        Err(error) => Err(error),
    }
}

fn verification_error_is_physical_damage(error: &TerminalIndexVerificationError) -> bool {
    matches!(
        error,
        TerminalIndexVerificationError::NoCompleteLayout { .. }
            | TerminalIndexVerificationError::ConflictingLayouts { .. }
            | TerminalIndexVerificationError::ConflictingReplicaEditions { .. }
            | TerminalIndexVerificationError::PrefixTruncated { .. }
            | TerminalIndexVerificationError::PrefixDamaged { .. }
            | TerminalIndexVerificationError::TapeFileCountMismatch { .. }
            | TerminalIndexVerificationError::TerminalComponentMismatch { .. }
            | TerminalIndexVerificationError::CanonicalMapMismatch { .. }
            | TerminalIndexVerificationError::Replica { .. }
            | TerminalIndexVerificationError::CrossReplicaConflict { .. }
            | TerminalIndexVerificationError::Separation { .. }
    )
}

fn verify_terminal_index_after_damage(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    initial_detail: String,
    authority: &mut Option<&mut dyn BotObjectRecoveryAuthority>,
) -> Result<TerminalIndexVerificationOutcome, TerminalIndexVerificationError> {
    let inventory = match read_terminal_index_inventory_summary(source, tape_uuid, block_size) {
        Ok(inventory) => inventory,
        Err(TerminalInventoryReadError::SelectedReplica {
            ordinal,
            source: error,
        }) => {
            let mut replicas = missing_evidence(
                "terminal member evidence became unavailable during inventory replay",
            );
            replicas[usize::from(ordinal - 1)] =
                TerminalReplicaEvidence::Invalid(TerminalReplicaFailure {
                    kind: TerminalReplicaFailureKind::PayloadInvalid,
                    detail: error.to_string(),
                });
            return terminal_recovery_required(
                source,
                tape_uuid,
                block_size,
                replicas,
                format!(
                    "terminal replica {ordinal} failed while replaying its payload after physical damage: {error}"
                ),
                authority,
            );
        }
        Err(error) => return Err(inventory_error_to_verification(error)),
    };
    let selection = match inventory {
        TerminalInventoryOutcome::Inventory(selection) => selection,
        TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery) => {
            return terminal_recovery_required(
                source,
                tape_uuid,
                block_size,
                recovery.replicas,
                initial_detail,
                authority,
            )
        }
    };
    let layout = selection.edition.descriptor.terminal_layout;
    source
        .configure_fixed_block_size(block_size)
        .map_err(|error| verification_source_error("configure fixed block size", error))?;
    let measured_eod = source
        .locate_end_of_data()
        .map_err(|error| verification_source_error("SPACE(EOD)", error))?;
    if measured_eod.partition != layout.partition || measured_eod.lba > layout.expected_eod_lba {
        return terminal_recovery_required(
            source,
            tape_uuid,
            block_size,
            selection.replicas,
            format!(
                "measured EOD partition {} LBA {} lies beyond planned terminal EOD partition {} LBA {}",
                measured_eod.partition,
                measured_eod.lba,
                layout.partition,
                layout.expected_eod_lba
            ),
            authority,
        );
    }
    let walked = scan_reconstruct_filemark_map_with_report(source, tape_uuid, block_size).map_err(
        |error| TerminalIndexVerificationError::PrefixWalk {
            message: error.to_string(),
        },
    )?;
    let prefix_count = layout.components[0].planned_tape_file_number;
    if walked
        .truncation
        .is_some_and(|truncation| truncation.tape_file_number < prefix_count)
        || walked
            .damaged_regions
            .iter()
            .any(|damage| damage.start.lba < layout.components[0].planned_start_lba)
    {
        return terminal_recovery_required(
            source,
            tape_uuid,
            block_size,
            selection.replicas,
            "physical damage or truncation lies inside the canonical pre-tail prefix".to_string(),
            authority,
        );
    }
    let prefix_len = usize::try_from(prefix_count).map_err(|_| {
        TerminalIndexVerificationError::ArithmeticOverflow {
            context: "degraded verification prefix count to usize",
        }
    })?;
    let Some(physical_prefix) = walked.map.entries().get(..prefix_len) else {
        return terminal_recovery_required(
            source,
            tape_uuid,
            block_size,
            selection.replicas,
            "BOT walk ended before the canonical pre-tail prefix".to_string(),
            authority,
        );
    };
    let verified_prefix_record_count = physical_prefix.iter().try_fold(0u64, |total, entry| {
        total.checked_add(entry.block_count).ok_or(
            TerminalIndexVerificationError::ArithmeticOverflow {
                context: "degraded verified prefix record count",
            },
        )
    })?;

    let mut replicas = missing_evidence("replica full verification was not attempted");
    let mut replica_editions: [Option<TapeIndexEditionPlan>; 3] = std::array::from_fn(|_| None);
    for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
        let index = usize::from(ordinal - 1);
        let envelope =
            match validate_member_envelope(source, tape_uuid, block_size, layout, ordinal) {
                Ok(envelope) => envelope,
                Err(TerminalMemberReadError::Invalid(failure)) => {
                    replicas[index] = TerminalReplicaEvidence::Invalid(failure);
                    continue;
                }
                Err(TerminalMemberReadError::Source(error)) => {
                    return Err(inventory_error_to_verification(error));
                }
            };
        let mut entry_index = 0usize;
        let result = validate_member_payload(
            source,
            block_size,
            &envelope,
            &mut |entry| {
                let Some(physical) = physical_prefix.get(entry_index) else {
                    return Err(TapeIndexReplicaError::Payload {
                        message: format!("canonical replica map emitted extra row {entry_index}"),
                    });
                };
                if !canonical_entry_matches_physical(entry, physical) {
                    return Err(TapeIndexReplicaError::Payload {
                        message: format!(
                            "canonical row {entry:?} disagrees with measured row {physical:?}"
                        ),
                    });
                }
                entry_index = entry_index.checked_add(1).ok_or(
                    TapeIndexReplicaError::ArithmeticOverflow {
                        context: "degraded verification canonical row index",
                    },
                )?;
                Ok(())
            },
            &mut |_| Ok(()),
        );
        replicas[index] = match result {
            Ok(summary) if entry_index == physical_prefix.len() => {
                replica_editions[index] = Some(envelope.header.plan.edition.clone());
                TerminalReplicaEvidence::Valid { summary }
            }
            Ok(_) => TerminalReplicaEvidence::Invalid(TerminalReplicaFailure {
                kind: TerminalReplicaFailureKind::PayloadInvalid,
                detail: format!(
                    "canonical map emitted {entry_index} rows, measured prefix has {}",
                    physical_prefix.len()
                ),
            }),
            Err(TerminalMemberReadError::Invalid(failure)) => {
                TerminalReplicaEvidence::Invalid(failure)
            }
            Err(TerminalMemberReadError::Source(error)) => {
                return Err(inventory_error_to_verification(error));
            }
        };
    }
    let selected_index = replicas
        .iter()
        .rposition(|evidence| matches!(evidence, TerminalReplicaEvidence::Valid { .. }));
    let Some(selected_index) = selected_index else {
        return terminal_recovery_required(
            source,
            tape_uuid,
            block_size,
            replicas,
            "no terminal replica survived full payload/canonical-prefix validation".to_string(),
            authority,
        );
    };
    let edition = replica_editions[selected_index]
        .clone()
        .expect("valid replica evidence records its edition");
    let selected_payload = match replicas[selected_index] {
        TerminalReplicaEvidence::Valid { summary } => summary,
        _ => unreachable!("selected index was derived from valid evidence"),
    };
    for index in 0..replicas.len() {
        if index == selected_index
            || !matches!(replicas[index], TerminalReplicaEvidence::Valid { .. })
        {
            continue;
        }
        let same_edition = replica_editions[index]
            .as_ref()
            .is_some_and(|candidate| *candidate == edition);
        let same_payload = matches!(
            replicas[index],
            TerminalReplicaEvidence::Valid { summary } if summary == selected_payload
        );
        if !same_edition || !same_payload {
            replicas[index] = TerminalReplicaEvidence::Invalid(TerminalReplicaFailure {
                kind: TerminalReplicaFailureKind::CrossSurvivorConflict,
                detail: format!(
                    "replica {} conflicts with selected physically valid replica {}",
                    index + 1,
                    selected_index + 1
                ),
            });
        }
    }

    let mut separations = std::array::from_fn(|_| TerminalSeparationEvidence::Invalid {
        detail: "separation verification was not attempted".to_string(),
    });
    for ordinal in 1..=crate::terminal_tail::TERMINAL_INDEX_SEPARATION_COUNT {
        let index = usize::from(ordinal - 1);
        match verify_separation_full(
            source,
            tape_uuid,
            block_size,
            layout,
            ordinal,
            edition.descriptor.edition_id,
        ) {
            Ok(interior_record_count) => {
                separations[index] = TerminalSeparationEvidence::Valid {
                    interior_record_count,
                };
            }
            Err(error @ TerminalIndexVerificationError::Separation { .. }) => {
                separations[index] = TerminalSeparationEvidence::Invalid {
                    detail: error.to_string(),
                };
            }
            Err(error) => return Err(error),
        }
    }
    let all_replicas_valid = replicas
        .iter()
        .all(|evidence| matches!(evidence, TerminalReplicaEvidence::Valid { .. }));
    let all_separations_valid = separations
        .iter()
        .all(|evidence| matches!(evidence, TerminalSeparationEvidence::Valid { .. }));
    let complete = all_replicas_valid
        && all_separations_valid
        && walked.truncation.is_none()
        && measured_eod.lba == layout.expected_eod_lba
        && walked.map.tape_file_count()
            == layout.components[TERMINAL_TAIL_COMPONENT_COUNT - 1]
                .planned_tape_file_number
                .checked_add(1)
                .ok_or(TerminalIndexVerificationError::ArithmeticOverflow {
                    context: "degraded verification complete file count",
                })?;
    let evidence = Box::new(TerminalIndexVerification {
        edition,
        selected_payload,
        replicas,
        separations,
        measured_eod,
        verified_prefix_tape_file_count: prefix_count,
        verified_prefix_record_count,
        measured_tape_file_count: walked.map.tape_file_count(),
    });
    Ok(if complete {
        TerminalIndexVerificationOutcome::VerifiedComplete(evidence)
    } else {
        TerminalIndexVerificationOutcome::VerifiedDegraded(evidence)
    })
}

fn terminal_recovery_required(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    replicas: [TerminalReplicaEvidence; 3],
    detail: String,
    authority: &mut Option<&mut dyn BotObjectRecoveryAuthority>,
) -> Result<TerminalIndexVerificationOutcome, TerminalIndexVerificationError> {
    source
        .configure_fixed_block_size(block_size)
        .map_err(|error| verification_source_error("configure fixed block size", error))?;
    let measured_eod = source
        .locate_end_of_data()
        .map_err(|error| verification_source_error("SPACE(EOD)", error))?;
    let bot_recovery = match authority.as_deref_mut() {
        Some(authority) => recover_terminal_inventory_from_bot_with_authority(
            source,
            tape_uuid,
            block_size,
            authority,
            |_| Ok(()),
        ),
        None => recover_terminal_inventory_from_bot(source, tape_uuid, block_size, |_| Ok(())),
    }
    .map_err(bot_recovery_verification_error)?;
    Ok(TerminalIndexVerificationOutcome::RecoveryRequired(
        Box::new(TerminalIndexRecoveryRequired {
            measured_eod,
            bot_recovery,
            replicas,
            detail,
        }),
    ))
}

fn bot_recovery_verification_error(
    error: BotStructuralRecoveryError,
) -> TerminalIndexVerificationError {
    match error {
        BotStructuralRecoveryError::Scan { message } => {
            TerminalIndexVerificationError::PrefixWalk { message }
        }
        other => TerminalIndexVerificationError::RecoveryAuthority {
            message: other.to_string(),
        },
    }
}

/// Strict complete-evidence pass used as the healthy fast path.
///
/// This operation measures EOD, walks every filemark-delimited tape file from
/// BOT, compares the measured pre-tail prefix with the canonical map embedded
/// in each replica, streams all three replica payloads, and reads every record
/// in both typed separation extents. Success therefore means all five terminal
/// components and the complete canonical prefix agreed physically.
fn verify_terminal_index_strict(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
) -> Result<TerminalIndexCompleteEvidence, TerminalIndexVerificationError> {
    validate_terminal_index_block_size_hint(block_size)?;
    source
        .configure_fixed_block_size(block_size)
        .map_err(|error| verification_source_error("configure fixed block size", error))?;
    let measured_eod = source
        .locate_end_of_data()
        .map_err(|error| verification_source_error("SPACE(EOD)", error))?;
    if measured_eod.partition != 0 {
        return Err(TerminalIndexVerificationError::Source {
            operation: "SPACE(EOD)",
            message: format!("returned unsupported partition {}", measured_eod.partition),
        });
    }

    let mut layouts = discover_terminal_layouts(source, tape_uuid, block_size, measured_eod)
        .map_err(inventory_error_to_verification)?
        .into_iter()
        .filter(|layout| layout.expected_eod_lba == measured_eod.lba)
        .collect::<Vec<_>>();
    layouts.dedup();
    let layout = match layouts.as_slice() {
        [] => {
            return Err(TerminalIndexVerificationError::NoCompleteLayout {
                measured_eod_lba: measured_eod.lba,
            })
        }
        [layout] => *layout,
        _ => {
            return Err(TerminalIndexVerificationError::ConflictingLayouts {
                count: layouts.len(),
            })
        }
    };

    let walked = scan_reconstruct_filemark_map_with_report(source, tape_uuid, block_size).map_err(
        |error| TerminalIndexVerificationError::PrefixWalk {
            message: error.to_string(),
        },
    )?;
    if let Some(truncation) = walked.truncation {
        return Err(TerminalIndexVerificationError::PrefixTruncated {
            tape_file_number: truncation.tape_file_number,
            kind: truncation.kind,
        });
    }
    if let Some(first) = walked.damaged_regions.first() {
        return Err(TerminalIndexVerificationError::PrefixDamaged {
            count: walked.damaged_regions.len(),
            first_kind: first.kind,
        });
    }

    let expected_file_count = layout.components[TERMINAL_TAIL_COMPONENT_COUNT - 1]
        .planned_tape_file_number
        .checked_add(1)
        .ok_or(TerminalIndexVerificationError::ArithmeticOverflow {
            context: "terminal physical tape-file count",
        })?;
    if walked.map.tape_file_count() != expected_file_count {
        return Err(TerminalIndexVerificationError::TapeFileCountMismatch {
            expected: expected_file_count,
            actual: walked.map.tape_file_count(),
        });
    }
    validate_measured_terminal_components(walked.map.entries(), &layout)?;

    let prefix_count = layout.components[0].planned_tape_file_number;
    let prefix_len = usize::try_from(prefix_count).map_err(|_| {
        TerminalIndexVerificationError::ArithmeticOverflow {
            context: "canonical prefix count to usize",
        }
    })?;
    let physical_prefix = walked.map.entries().get(..prefix_len).ok_or(
        TerminalIndexVerificationError::TapeFileCountMismatch {
            expected: expected_file_count,
            actual: walked.map.tape_file_count(),
        },
    )?;
    let verified_prefix_record_count = physical_prefix.iter().try_fold(0u64, |total, entry| {
        total.checked_add(entry.block_count).ok_or(
            TerminalIndexVerificationError::ArithmeticOverflow {
                context: "verified prefix record count",
            },
        )
    })?;

    let mut replica_summaries = Vec::with_capacity(usize::from(TERMINAL_INDEX_REPLICA_COUNT));
    let mut edition: Option<TapeIndexEditionPlan> = None;
    for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
        let envelope = validate_member_envelope(source, tape_uuid, block_size, layout, ordinal)
            .map_err(|error| member_error_to_verification(ordinal, error))?;
        let mut entry_index = 0usize;
        let mut canonical_mismatch = None;
        let summary = validate_member_payload(
            source,
            block_size,
            &envelope,
            &mut |entry| {
                let Some(physical) = physical_prefix.get(entry_index) else {
                    let detail = format!("canonical map emitted extra row at index {entry_index}");
                    canonical_mismatch = Some(detail.clone());
                    return Err(TapeIndexReplicaError::Payload { message: detail });
                };
                if !canonical_entry_matches_physical(entry, physical) {
                    canonical_mismatch = Some(format!(
                        "canonical row {entry:?} disagrees with measured row {physical:?}"
                    ));
                    return Err(TapeIndexReplicaError::Payload {
                        message: canonical_mismatch
                            .clone()
                            .expect("mismatch detail was just populated"),
                    });
                }
                entry_index = entry_index.checked_add(1).ok_or(
                    TapeIndexReplicaError::ArithmeticOverflow {
                        context: "full verification canonical row index",
                    },
                )?;
                Ok(())
            },
            &mut |_| Ok(()),
        )
        .map_err(|error| match error {
            TerminalMemberReadError::Source(error) => inventory_error_to_verification(error),
            TerminalMemberReadError::Invalid(failure) => {
                if let Some(detail) = canonical_mismatch {
                    TerminalIndexVerificationError::CanonicalMapMismatch {
                        ordinal,
                        tape_file_number: u64::try_from(entry_index).unwrap_or(u64::MAX),
                        detail,
                    }
                } else {
                    TerminalIndexVerificationError::Replica {
                        ordinal,
                        failure: failure.kind,
                        detail: failure.detail,
                    }
                }
            }
        })?;
        if entry_index != physical_prefix.len() {
            return Err(TerminalIndexVerificationError::CanonicalMapMismatch {
                ordinal,
                tape_file_number: u64::try_from(entry_index).unwrap_or(u64::MAX),
                detail: format!(
                    "canonical map emitted {entry_index} rows, measured prefix has {}",
                    physical_prefix.len()
                ),
            });
        }
        if let Some(reference) = edition.as_ref() {
            if envelope.header.plan.edition != *reference
                || replica_summaries
                    .first()
                    .is_some_and(|first| *first != summary)
            {
                return Err(TerminalIndexVerificationError::CrossReplicaConflict { ordinal });
            }
        } else {
            edition = Some(envelope.header.plan.edition.clone());
        }
        replica_summaries.push(summary);
    }

    let edition = edition.expect("three verified replicas establish one edition");
    let mut separation_interior_records = [0u64; 2];
    for ordinal in 1..=crate::terminal_tail::TERMINAL_INDEX_SEPARATION_COUNT {
        separation_interior_records[usize::from(ordinal - 1)] = verify_separation_full(
            source,
            tape_uuid,
            block_size,
            layout,
            ordinal,
            edition.descriptor.edition_id,
        )?;
    }

    let replicas: [TapeIndexReplicaPayloadSummary; 3] = replica_summaries
        .try_into()
        .expect("terminal replica count is exactly three");
    Ok(TerminalIndexCompleteEvidence {
        edition,
        replicas,
        separation_interior_records,
        measured_eod,
        verified_prefix_tape_file_count: prefix_count,
        verified_prefix_record_count,
        measured_tape_file_count: walked.map.tape_file_count(),
    })
}

fn validate_measured_terminal_components(
    entries: &[TapeFileMapEntry],
    layout: &TerminalTailLayout,
) -> Result<(), TerminalIndexVerificationError> {
    let expected_tape_file_count = layout.components[TERMINAL_TAIL_COMPONENT_COUNT - 1]
        .planned_tape_file_number
        .checked_add(1)
        .ok_or(TerminalIndexVerificationError::ArithmeticOverflow {
            context: "terminal planned tape-file count",
        })?;
    let actual_tape_file_count = u64::try_from(entries.len()).map_err(|_| {
        TerminalIndexVerificationError::ArithmeticOverflow {
            context: "terminal measured tape-file count",
        }
    })?;
    for component in layout.components {
        let index = usize::try_from(component.planned_tape_file_number).map_err(|_| {
            TerminalIndexVerificationError::ArithmeticOverflow {
                context: "terminal tape-file number to usize",
            }
        })?;
        let actual =
            entries
                .get(index)
                .ok_or(TerminalIndexVerificationError::TapeFileCountMismatch {
                    expected: expected_tape_file_count,
                    actual: actual_tape_file_count,
                })?;
        let expected_kind = match component.kind {
            crate::terminal_tail::TerminalTailComponentKind::TapeIndexReplica => {
                TapeFileKind::TapeIndexReplica
            }
            crate::terminal_tail::TerminalTailComponentKind::IndexSeparationExtent => {
                TapeFileKind::IndexSeparationExtent
            }
        };
        if actual.tape_file_number != component.planned_tape_file_number
            || actual.kind != expected_kind
            || actual.block_count != component.record_count
        {
            return Err(TerminalIndexVerificationError::TerminalComponentMismatch {
                tape_file_number: component.planned_tape_file_number,
                detail: format!(
                    "terminal component expected {expected_kind:?}/{} records, measured {:?}/{} records",
                    component.record_count, actual.kind, actual.block_count
                ),
            });
        }
    }
    Ok(())
}

fn canonical_entry_matches_physical(
    canonical: &TapeIndexReplicaMapEntry,
    physical: &TapeFileMapEntry,
) -> bool {
    let kind_matches = matches!(
        (canonical.kind, physical.kind),
        (TapeIndexReplicaFileKind::Object, TapeFileKind::Object)
            | (
                TapeIndexReplicaFileKind::ParitySidecar,
                TapeFileKind::ParitySidecar
            )
            | (TapeIndexReplicaFileKind::Bootstrap, TapeFileKind::Bootstrap)
            | (TapeIndexReplicaFileKind::ParityMap, TapeFileKind::ParityMap)
            | (
                TapeIndexReplicaFileKind::TapeIndexReplica,
                TapeFileKind::TapeIndexReplica
            )
            | (
                TapeIndexReplicaFileKind::IndexSeparationExtent,
                TapeFileKind::IndexSeparationExtent
            )
    );
    kind_matches
        && canonical.tape_file_number == physical.tape_file_number
        && canonical.block_count == physical.block_count
        && canonical.first_parity_data_ordinal == physical.first_parity_data_ordinal
        && canonical.protected_ordinal_start == physical.protected_ordinal_start
        && canonical.protected_ordinal_end_exclusive == physical.protected_ordinal_end_exclusive
        && canonical.epoch_id == physical.epoch_id
}

fn verify_separation_full(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    layout: TerminalTailLayout,
    ordinal: u16,
    expected_edition_id: [u8; 16],
) -> Result<u64, TerminalIndexVerificationError> {
    let component =
        layout
            .separation(ordinal)
            .map_err(|error| TerminalIndexVerificationError::Separation {
                ordinal,
                source: IndexSeparationError::Layout(error),
            })?;
    let header_block = read_fixed_block(
        source,
        PhysicalPositionHint {
            lba: component.planned_start_lba,
            partition: layout.partition,
        },
        block_size,
    )
    .map_err(|error| separation_read_error(ordinal, "read terminal separation header", error))?;
    let header = parse_index_separation_header(&header_block, tape_uuid)
        .map_err(|source| TerminalIndexVerificationError::Separation { ordinal, source })?;
    if header.plan.descriptor.edition_id != expected_edition_id {
        return Err(TerminalIndexVerificationError::Separation {
            ordinal,
            source: IndexSeparationError::DigestMismatch {
                field: "selected terminal edition id",
            },
        });
    }
    if header.plan.descriptor.gap_ordinal != ordinal
        || header.plan.descriptor.terminal_layout != layout
    {
        return Err(TerminalIndexVerificationError::Separation {
            ordinal,
            source: IndexSeparationError::DigestMismatch {
                field: "discovered terminal layout",
            },
        });
    }
    let footer_lba = component
        .planned_start_lba
        .checked_add(component.record_count.checked_sub(1).ok_or(
            TerminalIndexVerificationError::ArithmeticOverflow {
                context: "separation footer offset",
            },
        )?)
        .ok_or(TerminalIndexVerificationError::ArithmeticOverflow {
            context: "separation footer LBA",
        })?;
    let footer_block = read_fixed_block(
        source,
        PhysicalPositionHint {
            lba: footer_lba,
            partition: layout.partition,
        },
        block_size,
    )
    .map_err(|error| separation_read_error(ordinal, "read terminal separation footer", error))?;
    let footer = parse_index_separation_footer(&footer_block, tape_uuid)
        .map_err(|source| TerminalIndexVerificationError::Separation { ordinal, source })?;
    validate_index_separation_pair(&header, &footer)
        .map_err(|source| TerminalIndexVerificationError::Separation { ordinal, source })?;
    let interior_start = component.planned_start_lba.checked_add(1).ok_or(
        TerminalIndexVerificationError::ArithmeticOverflow {
            context: "separation interior start",
        },
    )?;
    let mut interior = RawSeparationInteriorSource {
        source,
        start: PhysicalPositionHint {
            lba: interior_start,
            partition: layout.partition,
        },
        block_size,
        record_count: component.record_count.checked_sub(2).ok_or(
            TerminalIndexVerificationError::ArithmeticOverflow {
                context: "separation interior record count",
            },
        )?,
        fatal_error: None,
    };
    let validation = validate_index_separation_full(&header, &footer, &mut interior);
    if let Some(error) = interior.fatal_error.take() {
        return Err(verification_source_error(
            "read terminal separation interior",
            error,
        ));
    }
    let verified = validation
        .map_err(|source| TerminalIndexVerificationError::Separation { ordinal, source })?;
    let filemark_lba = component
        .planned_start_lba
        .checked_add(component.record_count)
        .ok_or(TerminalIndexVerificationError::ArithmeticOverflow {
            context: "separation trailing filemark LBA",
        })?;
    expect_filemark(
        source,
        PhysicalPositionHint {
            lba: filemark_lba,
            partition: layout.partition,
        },
        block_size,
    )
    .map_err(|error| member_error_to_separation(ordinal, error))?;
    Ok(verified)
}

struct RawSeparationInteriorSource<'a> {
    source: &'a mut dyn RawTapeSource,
    start: PhysicalPositionHint,
    block_size: u32,
    record_count: u64,
    fatal_error: Option<ParityError>,
}

impl IndexSeparationInteriorBlockSource for RawSeparationInteriorSource<'_> {
    fn visit_interior_blocks(
        &mut self,
        visitor: &mut dyn FnMut(&[u8]) -> Result<(), IndexSeparationError>,
    ) -> Result<(), IndexSeparationError> {
        for offset in 0..self.record_count {
            let lba = self.start.lba.checked_add(offset).ok_or(
                IndexSeparationError::ArithmeticOverflow {
                    context: "full verification separation interior LBA",
                },
            )?;
            let block = match read_fixed_block(
                self.source,
                PhysicalPositionHint {
                    lba,
                    partition: self.start.partition,
                },
                self.block_size,
            ) {
                Ok(block) => block,
                Err(error) if terminal_candidate_error_is_damage(&error) => {
                    return Err(IndexSeparationError::PhysicalSource(error.to_string()));
                }
                Err(error) => {
                    let message = error.to_string();
                    self.fatal_error = Some(error);
                    return Err(IndexSeparationError::PhysicalSource(message));
                }
            };
            visitor(&block)?;
        }
        Ok(())
    }
}

fn discover_terminal_layouts(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    eod: PhysicalPositionHint,
) -> Result<Vec<TerminalTailLayout>, TerminalInventoryReadError> {
    source
        .locate_physical(eod)
        .map_err(|error| source_error("restore EOD position", error))?;
    let mut layouts = Vec::new();
    for _ in 0..TERMINAL_TAIL_COMPONENT_COUNT {
        let spaced = source
            .space_filemarks(-1)
            .map_err(|error| source_error("backspace terminal filemark", error))?;
        if spaced.filemarks_spaced != -1 || spaced.position_after.partition != eod.partition {
            break;
        }
        let Some(footer_lba) = spaced.position_after.lba.checked_sub(1) else {
            break;
        };
        let footer_position = PhysicalPositionHint {
            lba: footer_lba,
            partition: eod.partition,
        };
        match read_fixed_block(source, footer_position, block_size) {
            Ok(block) => {
                if let Ok(footer) = parse_tape_index_bootstrap_footer(&block, tape_uuid) {
                    if footer.observed_footer_lba == footer_lba {
                        let layout = footer.plan.edition.descriptor.terminal_layout;
                        if layout.expected_eod_lba >= eod.lba && !layouts.contains(&layout) {
                            layouts.push(layout);
                        }
                    }
                }
            }
            Err(error) if terminal_candidate_error_is_damage(&error) => {}
            Err(error) => return Err(source_error("read terminal footer candidate", error)),
        }
        source
            .locate_physical(spaced.position_after)
            .map_err(|error| source_error("restore backspaced filemark position", error))?;
    }
    Ok(layouts)
}

fn inspect_layout(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    layout: TerminalTailLayout,
) -> Result<LayoutInspection, TerminalInventoryReadError> {
    let mut replicas = missing_evidence("member was not inspected");
    let mut envelopes: [Option<ValidatedReplicaEnvelope>; 3] = [None, None, None];
    for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
        let index = usize::from(ordinal - 1);
        match validate_member_envelope(source, tape_uuid, block_size, layout, ordinal) {
            Ok(envelope) => {
                replicas[index] = TerminalReplicaEvidence::ConsistentEnvelope;
                envelopes[index] = Some(envelope);
            }
            Err(TerminalMemberReadError::Invalid(failure)) => {
                replicas[index] = TerminalReplicaEvidence::Invalid(failure);
            }
            Err(TerminalMemberReadError::Source(error)) => return Err(error),
        }
    }

    Ok(LayoutInspection {
        replicas,
        envelopes,
    })
}

fn validate_member_envelope(
    source: &mut dyn RawTapeSource,
    tape_uuid: &[u8; 16],
    block_size: u32,
    layout: TerminalTailLayout,
    ordinal: u16,
) -> Result<ValidatedReplicaEnvelope, TerminalMemberReadError> {
    let component = layout
        .replica(ordinal)
        .map_err(|error| invalid_member(TerminalReplicaFailureKind::Missing, error))?;
    let header_position = PhysicalPositionHint {
        lba: component.planned_start_lba,
        partition: layout.partition,
    };
    let header_block = read_fixed_block(source, header_position, block_size).map_err(|error| {
        member_read_error(
            "read terminal replica header",
            TerminalReplicaFailureKind::HeaderRead,
            error,
        )
    })?;
    let header = parse_tape_index_replica_header(&header_block, tape_uuid)
        .map_err(|error| invalid_member(TerminalReplicaFailureKind::HeaderInvalid, error))?;
    if header.plan.replica_ordinal != ordinal
        || header.plan.component != component
        || header.plan.edition.descriptor.terminal_layout != layout
    {
        return Err(invalid_member(
            TerminalReplicaFailureKind::HeaderInvalid,
            format!("header does not describe the discovered layout for replica {ordinal}"),
        ));
    }

    let footer_lba = component
        .planned_start_lba
        .checked_add(component.record_count.checked_sub(1).ok_or_else(|| {
            invalid_member(
                TerminalReplicaFailureKind::FooterRead,
                "replica record count has no footer",
            )
        })?)
        .ok_or_else(|| {
            invalid_member(
                TerminalReplicaFailureKind::FooterRead,
                "replica footer LBA overflows u64",
            )
        })?;
    let footer_block = read_fixed_block(
        source,
        PhysicalPositionHint {
            lba: footer_lba,
            partition: layout.partition,
        },
        block_size,
    )
    .map_err(|error| {
        member_read_error(
            "read terminal replica footer",
            TerminalReplicaFailureKind::FooterRead,
            error,
        )
    })?;
    let footer = parse_tape_index_bootstrap_footer(&footer_block, tape_uuid)
        .map_err(|error| invalid_member(TerminalReplicaFailureKind::FooterInvalid, error))?;
    if footer.observed_footer_lba != footer_lba || footer.plan.replica_ordinal != ordinal {
        return Err(invalid_member(
            TerminalReplicaFailureKind::FooterInvalid,
            format!("footer does not bind measured replica {ordinal} position"),
        ));
    }
    validate_tape_index_replica_pair(&header, &footer)
        .map_err(|error| invalid_member(TerminalReplicaFailureKind::LocalBinding, error))?;

    let filemark_lba = component
        .planned_start_lba
        .checked_add(component.record_count)
        .ok_or_else(|| {
            invalid_member(
                TerminalReplicaFailureKind::TrailingFilemark,
                "replica trailing-filemark LBA overflows u64",
            )
        })?;
    expect_filemark(
        source,
        PhysicalPositionHint {
            lba: filemark_lba,
            partition: layout.partition,
        },
        block_size,
    )?;

    Ok(ValidatedReplicaEnvelope { header, footer })
}

fn validate_member_payload<FE, FR>(
    source: &mut dyn RawTapeSource,
    block_size: u32,
    envelope: &ValidatedReplicaEnvelope,
    visit_entry: &mut FE,
    visit_row: &mut FR,
) -> Result<TapeIndexReplicaPayloadSummary, TerminalMemberReadError>
where
    FE: FnMut(&TapeIndexReplicaMapEntry) -> Result<(), TapeIndexReplicaError>,
    FR: FnMut(&TapeIndexReplicaObjectRow) -> Result<(), TapeIndexReplicaError>,
{
    let payload_start = envelope
        .header
        .plan
        .component
        .planned_start_lba
        .checked_add(1)
        .ok_or_else(|| {
            invalid_member(
                TerminalReplicaFailureKind::PayloadInvalid,
                "replica payload start overflows u64",
            )
        })?;
    let mut payload_source = RawReplicaPayloadSource {
        source,
        start: PhysicalPositionHint {
            lba: payload_start,
            partition: envelope
                .header
                .plan
                .edition
                .descriptor
                .terminal_layout
                .partition,
        },
        block_size,
        record_count: envelope
            .header
            .plan
            .edition
            .replica_layout
            .payload_record_count,
        fatal_error: None,
    };
    let result = validate_tape_index_replica_payload(
        &envelope.header,
        &envelope.footer,
        &mut payload_source,
        visit_entry,
        visit_row,
    );
    if let Some(error) = payload_source.fatal_error.take() {
        return Err(TerminalMemberReadError::Source(source_error(
            "read terminal replica payload",
            error,
        )));
    }
    result.map_err(|error| invalid_member(TerminalReplicaFailureKind::PayloadInvalid, error))
}

struct RawReplicaPayloadSource<'a> {
    source: &'a mut dyn RawTapeSource,
    start: PhysicalPositionHint,
    block_size: u32,
    record_count: u64,
    fatal_error: Option<ParityError>,
}

impl TapeIndexReplicaPayloadBlockSource for RawReplicaPayloadSource<'_> {
    fn visit_payload_blocks(
        &mut self,
        visitor: &mut dyn FnMut(&[u8]) -> Result<(), TapeIndexReplicaError>,
    ) -> Result<(), TapeIndexReplicaError> {
        for offset in 0..self.record_count {
            let lba = self.start.lba.checked_add(offset).ok_or(
                TapeIndexReplicaError::ArithmeticOverflow {
                    context: "terminal inventory payload LBA",
                },
            )?;
            let block = match read_fixed_block(
                self.source,
                PhysicalPositionHint {
                    lba,
                    partition: self.start.partition,
                },
                self.block_size,
            ) {
                Ok(block) => block,
                Err(error) if terminal_candidate_error_is_damage(&error) => {
                    return Err(TapeIndexReplicaError::Payload {
                        message: error.to_string(),
                    });
                }
                Err(error) => {
                    let message = error.to_string();
                    self.fatal_error = Some(error);
                    return Err(TapeIndexReplicaError::Payload { message });
                }
            };
            visitor(&block)?;
        }
        Ok(())
    }
}

fn expect_filemark(
    source: &mut dyn RawTapeSource,
    position: PhysicalPositionHint,
    block_size: u32,
) -> Result<(), TerminalMemberReadError> {
    source.locate_physical(position).map_err(|error| {
        member_read_error(
            "locate terminal replica trailing filemark",
            TerminalReplicaFailureKind::TrailingFilemark,
            error,
        )
    })?;
    let mut buffer = vec![
        0;
        usize::try_from(block_size).map_err(|_| {
            invalid_member(
                TerminalReplicaFailureKind::TrailingFilemark,
                "block size does not fit usize",
            )
        })?
    ];
    match source.read_record(&mut buffer) {
        Ok(RawReadOutcome::Filemark { position_after })
            if position_after.partition == position.partition
                && position_after.lba
                    == position.lba.checked_add(1).ok_or_else(|| {
                        invalid_member(
                            TerminalReplicaFailureKind::TrailingFilemark,
                            "position after filemark overflows u64",
                        )
                    })? =>
        {
            Ok(())
        }
        Ok(outcome) => Err(invalid_member(
            TerminalReplicaFailureKind::TrailingFilemark,
            format!(
                "expected filemark at LBA {}, observed {outcome:?}",
                position.lba
            ),
        )),
        Err(error) => Err(member_read_error(
            "read terminal replica trailing filemark",
            TerminalReplicaFailureKind::TrailingFilemark,
            error,
        )),
    }
}

fn read_fixed_block(
    source: &mut dyn RawTapeSource,
    position: PhysicalPositionHint,
    block_size: u32,
) -> Result<Vec<u8>, ParityError> {
    source.locate_physical(position)?;
    let expected = usize::try_from(block_size)
        .map_err(|_| ParityError::Invariant("terminal inventory block size does not fit usize"))?;
    let mut block = vec![0; expected];
    match source.read_record(&mut block)? {
        RawReadOutcome::Block {
            bytes,
            position_after,
        } if bytes == expected
            && position_after.partition == position.partition
            && position_after.lba
                == position.lba.checked_add(1).ok_or(ParityError::Invariant(
                    "terminal inventory post-read LBA overflows u64",
                ))? =>
        {
            Ok(block)
        }
        outcome => Err(ParityError::TapeIndexReplica(format!(
            "expected {expected}-byte block at partition {} LBA {}, observed {outcome:?}",
            position.partition, position.lba
        ))),
    }
}

fn missing_evidence(detail: &str) -> [TerminalReplicaEvidence; 3] {
    std::array::from_fn(|_| {
        TerminalReplicaEvidence::Invalid(TerminalReplicaFailure {
            kind: TerminalReplicaFailureKind::Missing,
            detail: detail.to_string(),
        })
    })
}

fn failure(
    kind: TerminalReplicaFailureKind,
    detail: impl std::fmt::Display,
) -> TerminalReplicaFailure {
    TerminalReplicaFailure {
        kind,
        detail: detail.to_string(),
    }
}

fn invalid_member(
    kind: TerminalReplicaFailureKind,
    detail: impl std::fmt::Display,
) -> TerminalMemberReadError {
    TerminalMemberReadError::Invalid(failure(kind, detail))
}

fn member_read_error(
    operation: &'static str,
    kind: TerminalReplicaFailureKind,
    error: ParityError,
) -> TerminalMemberReadError {
    if terminal_candidate_error_is_damage(&error) {
        invalid_member(kind, error)
    } else {
        TerminalMemberReadError::Source(source_error(operation, error))
    }
}

fn terminal_candidate_error_is_damage(error: &ParityError) -> bool {
    terminal_source_error_is_medium_damage(error)
        || matches!(error, ParityError::TapeIndexReplica(_))
}

fn terminal_source_error_is_medium_damage(error: &ParityError) -> bool {
    matches!(error, ParityError::TapeIo(error) if tape_error_is_current_medium_damage(error))
}

fn selected_member_error(
    ordinal: u16,
    error: TerminalMemberReadError,
) -> TerminalInventoryReadError {
    match error {
        TerminalMemberReadError::Invalid(failure) => TerminalInventoryReadError::SelectedReplica {
            ordinal,
            source: TapeIndexReplicaError::Payload {
                message: failure.detail,
            },
        },
        TerminalMemberReadError::Source(error) => error,
    }
}

fn member_error_to_verification(
    ordinal: u16,
    error: TerminalMemberReadError,
) -> TerminalIndexVerificationError {
    match error {
        TerminalMemberReadError::Invalid(failure) => TerminalIndexVerificationError::Replica {
            ordinal,
            failure: failure.kind,
            detail: failure.detail,
        },
        TerminalMemberReadError::Source(error) => inventory_error_to_verification(error),
    }
}

fn separation_read_error(
    ordinal: u16,
    operation: &'static str,
    error: ParityError,
) -> TerminalIndexVerificationError {
    if terminal_candidate_error_is_damage(&error) {
        TerminalIndexVerificationError::Separation {
            ordinal,
            source: IndexSeparationError::PhysicalSource(error.to_string()),
        }
    } else {
        verification_source_error(operation, error)
    }
}

fn member_error_to_separation(
    ordinal: u16,
    error: TerminalMemberReadError,
) -> TerminalIndexVerificationError {
    match error {
        TerminalMemberReadError::Invalid(failure) => TerminalIndexVerificationError::Separation {
            ordinal,
            source: IndexSeparationError::PhysicalSource(failure.detail),
        },
        TerminalMemberReadError::Source(error) => inventory_error_to_verification(error),
    }
}

fn source_error(operation: &'static str, error: ParityError) -> TerminalInventoryReadError {
    TerminalInventoryReadError::Source {
        operation,
        message: error.to_string(),
    }
}

fn verification_source_error(
    operation: &'static str,
    error: ParityError,
) -> TerminalIndexVerificationError {
    TerminalIndexVerificationError::Source {
        operation,
        message: error.to_string(),
    }
}

fn inventory_error_to_verification(
    error: TerminalInventoryReadError,
) -> TerminalIndexVerificationError {
    match error {
        TerminalInventoryReadError::BlockSize(error) => {
            TerminalIndexVerificationError::BlockSize(error)
        }
        TerminalInventoryReadError::Source { operation, message } => {
            TerminalIndexVerificationError::Source { operation, message }
        }
        TerminalInventoryReadError::SelectedReplica { ordinal, source } => {
            TerminalIndexVerificationError::Replica {
                ordinal,
                failure: TerminalReplicaFailureKind::PayloadInvalid,
                detail: source.to_string(),
            }
        }
        TerminalInventoryReadError::StreamVisitor { message } => {
            TerminalIndexVerificationError::Source {
                operation: "terminal inventory visitor",
                message,
            }
        }
        TerminalInventoryReadError::TerminalIndexReplicaConflict { count } => {
            TerminalIndexVerificationError::ConflictingReplicaEditions { count }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape_index_replica::{
        checked_tape_index_replica_layout, plan_tape_index_edition, plan_tape_index_replica,
        write_tape_index_replica, TapeIndexEditionDescriptor, TapeIndexReplicaObservation,
    };
    use crate::{
        recover_terminal_inventory_from_bot_controlled,
        recover_terminal_inventory_from_bot_with_authority, BotObjectRecoveryAuthority,
        BotObjectRecoveryAuthorityRow, BotObjectRecoveryAuthorityScope, BotStructuralRecoveryEvent,
        ScanWalkControl, TapeIndexReplicaCounts, TapeIndexReplicaFileKind,
        TapeIndexReplicaMapEntry, TapeIndexReplicaObjectRow, TapeIndexReplicaRecordSource,
        TapeIndexReplicaScope,
    };

    const BLOCK_SIZE: u32 = 256 * 1024;
    const TAPE_UUID: [u8; 16] = [0x61; 16];

    #[derive(Clone)]
    struct BotOnlyRows;

    struct TestBotAuthority {
        scope: BotObjectRecoveryAuthorityScope,
        second_scope: Option<BotObjectRecoveryAuthorityScope>,
        fail_after_rows_on_second_visit: Option<usize>,
        rows: Vec<BotObjectRecoveryAuthorityRow>,
        visits: u64,
    }

    impl BotObjectRecoveryAuthority for TestBotAuthority {
        fn visit_object_rows(
            &mut self,
            visitor: &mut dyn FnMut(
                &BotObjectRecoveryAuthorityRow,
            ) -> Result<(), BotStructuralRecoveryError>,
        ) -> Result<BotObjectRecoveryAuthorityScope, BotStructuralRecoveryError> {
            self.visits += 1;
            for (index, row) in self.rows.iter().enumerate() {
                visitor(row)?;
                if self.visits == 2 && self.fail_after_rows_on_second_visit == Some(index + 1) {
                    return Err(BotStructuralRecoveryError::ObjectAuthority {
                        message: "injected late second authority replay failure".to_string(),
                    });
                }
            }
            Ok(if self.visits == 2 {
                self.second_scope.unwrap_or(self.scope)
            } else {
                self.scope
            })
        }
    }

    impl TapeIndexReplicaRecordSource for BotOnlyRows {
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

    #[derive(Clone)]
    enum Record {
        Block(Vec<u8>),
        Filemark,
    }

    struct RecordingSource {
        records: Vec<Record>,
        cursor: usize,
        read_lbas: Vec<u64>,
        eod_calls: u64,
        read_fault: Option<(u64, TestReadFault)>,
        configure_fault: Option<TestReadFault>,
        locate_fault: Option<(u64, TestReadFault)>,
    }

    #[derive(Clone, Copy)]
    enum TestReadFault {
        Medium,
        DeferredFixedMedium,
        DeferredDescriptorMedium,
        Hardware,
        Transport,
    }

    impl TestReadFault {
        fn error(self) -> ParityError {
            ParityError::TapeIo(match self {
                Self::Medium => TapeIoError::CheckCondition(
                    remanence_library::scsi::ScsiError::CheckCondition {
                        sense: vec![0x72, 0x03, 0x11, 0x00],
                        bytes_transferred: 0,
                    },
                ),
                Self::DeferredFixedMedium => TapeIoError::CheckCondition(
                    remanence_library::scsi::ScsiError::CheckCondition {
                        sense: vec![0x71, 0x00, 0x03, 0, 0, 0, 0, 10, 0, 0, 0, 0, 0x11, 0],
                        bytes_transferred: 0,
                    },
                ),
                Self::DeferredDescriptorMedium => TapeIoError::CheckCondition(
                    remanence_library::scsi::ScsiError::CheckCondition {
                        sense: vec![0x73, 0x03, 0x11, 0x00],
                        bytes_transferred: 0,
                    },
                ),
                Self::Hardware => TapeIoError::CheckCondition(
                    remanence_library::scsi::ScsiError::CheckCondition {
                        sense: vec![0x72, 0x04, 0x44, 0x00],
                        bytes_transferred: 0,
                    },
                ),
                Self::Transport => {
                    TapeIoError::Transport(remanence_library::scsi::ScsiError::TransportError {
                        status: 0,
                        host_status: 0,
                        driver_status: 0x06,
                        info: 1,
                        sense: Vec::new(),
                    })
                }
            })
        }
    }

    impl RecordingSource {
        fn new(records: Vec<Record>) -> Self {
            Self {
                records,
                cursor: 0,
                read_lbas: Vec::new(),
                eod_calls: 0,
                read_fault: None,
                configure_fault: None,
                locate_fault: None,
            }
        }

        fn with_read_fault(mut self, lba: u64, fault: TestReadFault) -> Self {
            self.read_fault = Some((lba, fault));
            self
        }

        fn with_configure_fault(mut self, fault: TestReadFault) -> Self {
            self.configure_fault = Some(fault);
            self
        }

        fn with_locate_fault(mut self, lba: u64, fault: TestReadFault) -> Self {
            self.locate_fault = Some((lba, fault));
            self
        }
    }

    impl RawTapeSource for RecordingSource {
        fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
            if let Some(fault) = self.configure_fault {
                return Err(fault.error());
            }
            if block_size != BLOCK_SIZE {
                return Err(ParityError::Invariant("unexpected test block size"));
            }
            Ok(())
        }

        fn locate_physical(&mut self, hint: PhysicalPositionHint) -> Result<(), ParityError> {
            if let Some((fault_lba, fault)) = self.locate_fault {
                if fault_lba == hint.lba {
                    return Err(fault.error());
                }
            }
            if hint.partition != 0 {
                return Err(ParityError::Invariant("unexpected test partition"));
            }
            self.cursor = usize::try_from(hint.lba)
                .map_err(|_| ParityError::Invariant("test LBA does not fit usize"))?
                .min(self.records.len());
            Ok(())
        }

        fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            self.eod_calls = self
                .eod_calls
                .checked_add(1)
                .ok_or(ParityError::Invariant("test EOD call count overflows"))?;
            self.cursor = self.records.len();
            Ok(PhysicalPositionHint::new(
                u64::try_from(self.cursor)
                    .map_err(|_| ParityError::Invariant("test EOD does not fit u64"))?,
            ))
        }

        fn space_filemarks(
            &mut self,
            count: i64,
        ) -> Result<crate::SpaceFilemarksOutcome, ParityError> {
            let mut spaced = 0i64;
            if count >= 0 {
                while self.cursor < self.records.len() && spaced < count {
                    if matches!(self.records[self.cursor], Record::Filemark) {
                        spaced += 1;
                    }
                    self.cursor += 1;
                }
            } else {
                while self.cursor > 0 && spaced > count {
                    self.cursor -= 1;
                    if matches!(self.records[self.cursor], Record::Filemark) {
                        spaced -= 1;
                    }
                }
            }
            Ok(crate::SpaceFilemarksOutcome {
                filemarks_spaced: spaced,
                position_after: PhysicalPositionHint::new(
                    u64::try_from(self.cursor)
                        .map_err(|_| ParityError::Invariant("test cursor does not fit u64"))?,
                ),
                hit_end_of_data: spaced != count,
            })
        }

        fn read_record(&mut self, buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
            let lba = u64::try_from(self.cursor)
                .map_err(|_| ParityError::Invariant("test cursor does not fit u64"))?;
            self.read_lbas.push(lba);
            if let Some((fault_lba, fault)) = self.read_fault {
                if fault_lba == lba {
                    return Err(fault.error());
                }
            }
            let Some(record) = self.records.get(self.cursor) else {
                return Ok(RawReadOutcome::EndOfData {
                    position_after: PhysicalPositionHint::new(lba),
                });
            };
            self.cursor = self
                .cursor
                .checked_add(1)
                .ok_or(ParityError::Invariant("test cursor overflows"))?;
            let position_after = PhysicalPositionHint::new(
                lba.checked_add(1)
                    .ok_or(ParityError::Invariant("test post-read LBA overflows"))?,
            );
            match record {
                Record::Block(block) => {
                    if block.len() != buf.len() {
                        return Err(ParityError::Invariant("test record has wrong size"));
                    }
                    buf.copy_from_slice(block);
                    Ok(RawReadOutcome::Block {
                        bytes: block.len(),
                        position_after,
                    })
                }
                Record::Filemark => Ok(RawReadOutcome::Filemark { position_after }),
            }
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
            Ok(PhysicalPositionHint::new(
                u64::try_from(self.cursor)
                    .map_err(|_| ParityError::Invariant("test cursor does not fit u64"))?,
            ))
        }
    }

    struct TripleFixture {
        records: Vec<Record>,
        layout: TerminalTailLayout,
    }

    fn edition_plan(layout: TerminalTailLayout, edition_id: [u8; 16]) -> TapeIndexEditionPlan {
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: 1,
            object_row_count: 0,
        };
        let mut rows = BotOnlyRows;
        plan_tape_index_edition(
            TapeIndexEditionDescriptor {
                tape_uuid: TAPE_UUID,
                edition_id,
                edition_sequence: 9,
                scope: TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 1,
                    total_data_ordinals: 0,
                    highest_protected_ordinal: 0,
                },
                counts,
                block_size: BLOCK_SIZE,
                compression_enabled: false,
                writer_version: "terminal-inventory-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: layout,
            },
            &mut rows,
        )
        .expect("edition plan")
    }

    fn triple_fixture() -> TripleFixture {
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: 1,
            object_row_count: 0,
        };
        let replica_records = checked_tape_index_replica_layout(BLOCK_SIZE, counts)
            .expect("replica geometry")
            .replica_record_count;
        let layout = TerminalTailLayout::new(0, BLOCK_SIZE, 1, 2, replica_records, 3)
            .expect("terminal layout");
        let mut rows = BotOnlyRows;
        let edition = edition_plan(layout, [0x72; 16]);

        let mut bootstrap = vec![0u8; BLOCK_SIZE as usize];
        crate::bootstrap::write_bootstrap_block(
            &crate::BootstrapPayload {
                scheme: None,
                no_parity_flag: true,
                filemark_map_digest: None,
                tape_uuid: TAPE_UUID,
                written_by_version: "terminal-inventory-test".to_string(),
                written_at: "2026-08-09T00:00:00Z".to_string(),
                sequence: 0,
                block_size_bytes: BLOCK_SIZE,
                drive_compression: false,
            },
            &mut bootstrap,
        )
        .expect("BOT bootstrap bytes");
        let mut files = vec![vec![bootstrap]];
        for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
            let plan = plan_tape_index_replica(edition.clone(), ordinal).expect("replica plan");
            let mut blocks = Vec::new();
            write_tape_index_replica(
                &plan,
                TapeIndexReplicaObservation {
                    tape_file_number: plan.component.planned_tape_file_number,
                    start_lba: plan.component.planned_start_lba,
                    record_count: plan.component.record_count,
                },
                &mut rows,
                |block| {
                    blocks.push(block.to_vec());
                    Ok(())
                },
            )
            .expect("replica bytes");
            files.push(blocks);
            if ordinal != TERMINAL_INDEX_REPLICA_COUNT {
                let gap_ordinal = ordinal;
                let gap_plan = crate::plan_index_separation(crate::IndexSeparationDescriptor {
                    tape_uuid: TAPE_UUID,
                    edition_id: edition.descriptor.edition_id,
                    gap_ordinal,
                    block_size: BLOCK_SIZE,
                    nominal_extent_bytes: u64::from(BLOCK_SIZE) * 3,
                    total_records: 3,
                    compression_enabled: false,
                    terminal_layout: layout,
                })
                .expect("separation plan");
                let mut gap_blocks = Vec::new();
                crate::write_index_separation(
                    &gap_plan,
                    crate::IndexSeparationObservation {
                        tape_file_number: gap_plan.component.planned_tape_file_number,
                        start_lba: gap_plan.component.planned_start_lba,
                        record_count: gap_plan.component.record_count,
                    },
                    |block| {
                        gap_blocks.push(block.to_vec());
                        Ok(())
                    },
                )
                .expect("separation bytes");
                files.push(gap_blocks);
            }
        }
        let mut records = Vec::new();
        for file in files {
            records.extend(file.into_iter().map(Record::Block));
            records.push(Record::Filemark);
        }
        assert_eq!(
            u64::try_from(records.len()).expect("fixture length"),
            layout.expected_eod_lba
        );
        TripleFixture { records, layout }
    }

    fn bot_bootstrap() -> Vec<u8> {
        let mut block = vec![0u8; BLOCK_SIZE as usize];
        crate::bootstrap::write_bootstrap_block(
            &crate::BootstrapPayload {
                scheme: None,
                no_parity_flag: true,
                filemark_map_digest: None,
                tape_uuid: TAPE_UUID,
                written_by_version: "terminal-recovery-test".to_string(),
                written_at: "2026-08-09T00:00:00Z".to_string(),
                sequence: 0,
                block_size_bytes: BLOCK_SIZE,
                drive_compression: false,
            },
            &mut block,
        )
        .expect("BOT recovery bootstrap bytes");
        block
    }

    fn corrupt_replica_payload(fixture: &mut TripleFixture, ordinal: u16) {
        let component = fixture.layout.replica(ordinal).expect("replica component");
        let payload_lba = component
            .planned_start_lba
            .checked_add(1)
            .expect("payload LBA");
        let Record::Block(block) =
            &mut fixture.records[usize::try_from(payload_lba).expect("payload index")]
        else {
            panic!("payload record must be a block")
        };
        block[5] ^= 0x80;
    }

    fn corrupt_separation_interior(fixture: &mut TripleFixture, ordinal: u16) {
        let component = fixture
            .layout
            .separation(ordinal)
            .expect("separation component");
        let interior_lba = component
            .planned_start_lba
            .checked_add(1)
            .expect("separation interior LBA");
        let Record::Block(block) =
            &mut fixture.records[usize::try_from(interior_lba).expect("interior index")]
        else {
            panic!("separation interior must be a block")
        };
        block[BLOCK_SIZE as usize - 1] = 0x7F;
    }

    fn replace_separation_edition(fixture: &mut TripleFixture, ordinal: u16, edition_id: [u8; 16]) {
        let component = fixture
            .layout
            .separation(ordinal)
            .expect("separation component");
        let plan = crate::plan_index_separation(crate::IndexSeparationDescriptor {
            tape_uuid: TAPE_UUID,
            edition_id,
            gap_ordinal: ordinal,
            block_size: BLOCK_SIZE,
            nominal_extent_bytes: u64::from(BLOCK_SIZE) * component.record_count,
            total_records: component.record_count,
            compression_enabled: false,
            terminal_layout: fixture.layout,
        })
        .expect("replacement separation plan");
        let mut blocks = Vec::new();
        crate::write_index_separation(
            &plan,
            crate::IndexSeparationObservation {
                tape_file_number: component.planned_tape_file_number,
                start_lba: component.planned_start_lba,
                record_count: component.record_count,
            },
            |block| {
                blocks.push(block.to_vec());
                Ok(())
            },
        )
        .expect("replacement separation bytes");
        for (offset, block) in blocks.into_iter().enumerate() {
            let lba = component
                .planned_start_lba
                .checked_add(u64::try_from(offset).expect("replacement offset fits u64"))
                .expect("replacement separation LBA");
            fixture.records[usize::try_from(lba).expect("replacement index")] =
                Record::Block(block);
        }
    }

    fn replace_replica_edition(fixture: &mut TripleFixture, ordinal: u16, edition_id: [u8; 16]) {
        let mut rows = BotOnlyRows;
        let plan = plan_tape_index_replica(edition_plan(fixture.layout, edition_id), ordinal)
            .expect("replacement replica plan");
        let mut blocks = Vec::new();
        write_tape_index_replica(
            &plan,
            TapeIndexReplicaObservation {
                tape_file_number: plan.component.planned_tape_file_number,
                start_lba: plan.component.planned_start_lba,
                record_count: plan.component.record_count,
            },
            &mut rows,
            |block| {
                blocks.push(block.to_vec());
                Ok(())
            },
        )
        .expect("replacement replica bytes");
        for (offset, block) in blocks.into_iter().enumerate() {
            let lba = plan
                .component
                .planned_start_lba
                .checked_add(u64::try_from(offset).expect("replacement offset"))
                .expect("replacement LBA");
            fixture.records[usize::try_from(lba).expect("replacement index")] =
                Record::Block(block);
        }
    }

    #[test]
    fn healthy_summary_inventory_reads_c_once_and_never_reads_the_prefix() {
        let fixture = triple_fixture();
        let mut source = RecordingSource::new(fixture.records);
        let outcome = read_terminal_index_inventory_summary(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("healthy inventory");
        let TerminalInventoryOutcome::Inventory(selection) = outcome else {
            panic!("healthy triple must return inventory")
        };
        assert_eq!(selection.selected_replica_ordinal, 3);
        assert!(!selection.is_degraded());
        assert_eq!(source.eod_calls, 1);
        assert!(source.read_lbas.iter().all(|lba| *lba >= 2));
        for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
            let payload_lba = fixture
                .layout
                .replica(ordinal)
                .expect("replica")
                .planned_start_lba
                .checked_add(1)
                .expect("payload LBA");
            assert_eq!(
                source
                    .read_lbas
                    .iter()
                    .filter(|lba| **lba == payload_lba)
                    .count(),
                usize::from(ordinal == TERMINAL_INDEX_REPLICA_COUNT),
                "healthy fast inventory must stream exactly one body, replica C"
            );
        }
    }

    #[test]
    fn full_verify_walks_prefix_and_validates_three_replicas_and_two_gaps() {
        let fixture = triple_fixture();
        let expected_eod = fixture.layout.expected_eod_lba;
        let mut source = RecordingSource::new(fixture.records);
        let outcome = verify_terminal_index_full(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("complete physical verification");
        let TerminalIndexVerificationOutcome::VerifiedComplete(verification) = outcome else {
            panic!("healthy physical tape must verify complete")
        };

        assert_eq!(verification.measured_eod.lba, expected_eod);
        assert_eq!(verification.verified_prefix_tape_file_count, 1);
        assert_eq!(verification.verified_prefix_record_count, 1);
        assert_eq!(verification.measured_tape_file_count, 6);
        assert!(verification.separations.iter().all(|evidence| matches!(
            evidence,
            TerminalSeparationEvidence::Valid {
                interior_record_count: 1
            }
        )));
        assert!(verification
            .replicas
            .windows(2)
            .all(|pair| pair[0] == pair[1]));
        assert!(source.read_lbas.contains(&0), "full verify must walk BOT");
        for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
            let payload_lba = fixture
                .layout
                .replica(ordinal)
                .expect("replica")
                .planned_start_lba
                .checked_add(1)
                .expect("payload LBA");
            assert!(
                source.read_lbas.contains(&payload_lba),
                "full verify must stream replica {ordinal}"
            );
        }
        for ordinal in 1..=crate::TERMINAL_INDEX_SEPARATION_COUNT {
            let interior_lba = fixture
                .layout
                .separation(ordinal)
                .expect("separation")
                .planned_start_lba
                .checked_add(1)
                .expect("interior LBA");
            assert!(
                source.read_lbas.contains(&interior_lba),
                "full verify must read gap {ordinal} interior"
            );
        }
    }

    #[test]
    fn full_verify_reports_one_corrupt_replica_as_verified_degraded() {
        let mut fixture = triple_fixture();
        corrupt_replica_payload(&mut fixture, 1);
        let mut source = RecordingSource::new(fixture.records);
        let outcome = verify_terminal_index_full(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("physical integrity damage is a typed outcome");
        let TerminalIndexVerificationOutcome::VerifiedDegraded(verification) = outcome else {
            panic!("one corrupt redundant replica must verify degraded")
        };
        assert!(matches!(
            verification.replicas[0],
            TerminalReplicaEvidence::Invalid(_)
        ));
        assert!(verification.replicas[1..]
            .iter()
            .all(|evidence| matches!(evidence, TerminalReplicaEvidence::Valid { .. })));
    }

    #[test]
    fn full_verify_after_replica_damage_keeps_medium_separation_damage_typed() {
        let mut fixture = triple_fixture();
        corrupt_replica_payload(&mut fixture, 1);
        let separation_lba = fixture
            .layout
            .separation(1)
            .expect("AB separation")
            .planned_start_lba
            .checked_add(1)
            .expect("AB separation interior LBA");
        let mut source = RecordingSource::new(fixture.records)
            .with_read_fault(separation_lba, TestReadFault::Medium);

        let outcome = verify_terminal_index_full(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("medium damage remains typed degraded evidence");
        assert!(matches!(
            outcome,
            TerminalIndexVerificationOutcome::VerifiedDegraded(verification)
                if matches!(
                    verification.separations[0],
                    TerminalSeparationEvidence::Invalid { .. }
                )
        ));
    }

    #[test]
    fn full_verify_after_replica_damage_propagates_non_medium_separation_failures() {
        for fault in [
            TestReadFault::DeferredFixedMedium,
            TestReadFault::DeferredDescriptorMedium,
            TestReadFault::Hardware,
            TestReadFault::Transport,
        ] {
            let mut fixture = triple_fixture();
            corrupt_replica_payload(&mut fixture, 1);
            let separation_lba = fixture
                .layout
                .separation(1)
                .expect("AB separation")
                .planned_start_lba
                .checked_add(1)
                .expect("AB separation interior LBA");
            let mut source =
                RecordingSource::new(fixture.records).with_read_fault(separation_lba, fault);

            let error = verify_terminal_index_full(&mut source, &TAPE_UUID, BLOCK_SIZE)
                .expect_err("non-medium separation failure must abort degraded verification");
            assert!(matches!(
                error,
                TerminalIndexVerificationError::Source {
                    operation: "read terminal separation interior",
                    ..
                }
            ));
        }
    }

    #[test]
    fn full_verify_reports_nonzero_separation_interior_as_degraded() {
        let mut fixture = triple_fixture();
        corrupt_separation_interior(&mut fixture, 2);
        let mut source = RecordingSource::new(fixture.records);
        let outcome = verify_terminal_index_full(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("separation damage is a typed outcome");
        assert!(matches!(
            outcome,
            TerminalIndexVerificationOutcome::VerifiedDegraded(verification)
                if matches!(verification.separations[1], TerminalSeparationEvidence::Invalid { .. })
        ));
    }

    #[test]
    fn full_verify_rejects_self_consistent_separation_from_another_edition() {
        let mut fixture = triple_fixture();
        replace_separation_edition(&mut fixture, 1, [0x99; 16]);
        let mut source = RecordingSource::new(fixture.records);
        let outcome = verify_terminal_index_full(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("edition mismatch is retained as typed degraded evidence");
        assert!(matches!(
            outcome,
            TerminalIndexVerificationOutcome::VerifiedDegraded(verification)
                if matches!(
                    &verification.separations[0],
                    TerminalSeparationEvidence::Invalid { detail }
                        if detail.contains("selected terminal edition id")
                )
        ));
    }

    #[test]
    fn full_verify_rejects_physical_data_after_planned_eod() {
        let mut fixture = triple_fixture();
        fixture
            .records
            .push(Record::Block(vec![0xEE; BLOCK_SIZE as usize]));
        let mut source = RecordingSource::new(fixture.records);
        let outcome = verify_terminal_index_full(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("untrusted tail returns typed recovery evidence");
        assert!(matches!(
            outcome,
            TerminalIndexVerificationOutcome::RecoveryRequired(_)
        ));
    }

    #[test]
    fn full_verify_all_invalid_runs_bot_recovery_with_measured_eod() {
        let mut fixture = triple_fixture();
        for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
            corrupt_replica_payload(&mut fixture, ordinal);
        }
        let expected_eod = fixture.layout.expected_eod_lba;
        let mut source = RecordingSource::new(fixture.records);
        let outcome = verify_terminal_index_full(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("semantic damage must return typed recovery evidence");
        let TerminalIndexVerificationOutcome::RecoveryRequired(recovery) = outcome else {
            panic!("no canonical survivor must require structural recovery")
        };

        assert_eq!(recovery.measured_eod.lba, expected_eod);
        assert!(recovery.bot_recovery.structural_entry_count > 0);
        assert!(recovery
            .replicas
            .iter()
            .all(|evidence| matches!(evidence, TerminalReplicaEvidence::Invalid(_))));
        assert!(
            source.read_lbas.contains(&0),
            "recovery must perform a BOT walk"
        );
    }

    #[test]
    fn bot_recovery_classifies_recovered_unknown_and_preserves_torn_control() {
        let terminal = triple_fixture();
        let replica_a_start = terminal
            .layout
            .replica(1)
            .expect("replica A")
            .planned_start_lba;
        let torn_control =
            terminal.records[usize::try_from(replica_a_start).expect("replica A index")].clone();
        let bootstrap = bot_bootstrap();
        let records = vec![
            Record::Block(bootstrap),
            Record::Filemark,
            Record::Block(vec![0x10; BLOCK_SIZE as usize]),
            Record::Block(vec![0x11; BLOCK_SIZE as usize]),
            Record::Filemark,
            Record::Block(vec![0x20; BLOCK_SIZE as usize]),
            Record::Filemark,
            torn_control,
        ];
        let mut source = RecordingSource::new(records);
        let mut authority = TestBotAuthority {
            scope: BotObjectRecoveryAuthorityScope {
                tape_uuid: TAPE_UUID,
                block_size: BLOCK_SIZE,
                covered_prefix_tape_file_count: 2,
                object_row_count: 1,
            },
            second_scope: None,
            fail_after_rows_on_second_visit: None,
            rows: vec![BotObjectRecoveryAuthorityRow {
                tape_file_number: 1,
                stored_block_count: 2,
                object_id: b"recovered-object".to_vec(),
            }],
            visits: 0,
        };
        let mut objects = Vec::new();
        let summary = recover_terminal_inventory_from_bot_with_authority(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            &mut authority,
            |object| {
                objects.push(object.clone());
                Ok(())
            },
        )
        .expect("BOT recovery");

        assert_eq!(summary.structural_entry_count, 3);
        assert_eq!(summary.complete_object_count, 2);
        assert_eq!(summary.recovered_object_count, 1);
        assert_eq!(summary.unknown_object_count, 1);
        assert_eq!(summary.incomplete_object_count, 0);
        assert_eq!(authority.visits, 2);
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].state, BotRecoveredObjectState::Recovered);
        assert_eq!(
            objects[0].object_id.as_deref(),
            Some(b"recovered-object".as_slice())
        );
        assert_eq!(objects[1].state, BotRecoveredObjectState::Unknown);
        assert!(objects[1].object_id.is_none());
        assert!(objects.iter().all(|object| object.tape_file_number < 3));
    }

    #[test]
    fn bot_recovery_rejects_authority_geometry_before_emitting_objects() {
        let records = vec![
            Record::Block(bot_bootstrap()),
            Record::Filemark,
            Record::Block(vec![0x10; BLOCK_SIZE as usize]),
            Record::Filemark,
        ];
        let mut source = RecordingSource::new(records);
        let mut authority = TestBotAuthority {
            scope: BotObjectRecoveryAuthorityScope {
                tape_uuid: TAPE_UUID,
                block_size: BLOCK_SIZE,
                covered_prefix_tape_file_count: 2,
                object_row_count: 1,
            },
            second_scope: None,
            fail_after_rows_on_second_visit: None,
            rows: vec![BotObjectRecoveryAuthorityRow {
                tape_file_number: 1,
                stored_block_count: 2,
                object_id: b"wrong-geometry".to_vec(),
            }],
            visits: 0,
        };
        let mut objects = Vec::new();
        let error = recover_terminal_inventory_from_bot_with_authority(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            &mut authority,
            |object| {
                objects.push(object.clone());
                Ok(())
            },
        )
        .expect_err("mismatched checkpoint geometry must fail closed");

        assert!(matches!(
            error,
            BotStructuralRecoveryError::ConflictingObjectAuthority {
                tape_file_number: 1,
                ..
            }
        ));
        assert!(objects.is_empty());
        assert_eq!(authority.visits, 1);
    }

    #[test]
    fn bot_recovery_rejects_missing_authority_row_before_emitting_objects() {
        let records = vec![
            Record::Block(bot_bootstrap()),
            Record::Filemark,
            Record::Block(vec![0x10; BLOCK_SIZE as usize]),
            Record::Filemark,
        ];
        let mut source = RecordingSource::new(records);
        let mut authority = TestBotAuthority {
            scope: BotObjectRecoveryAuthorityScope {
                tape_uuid: TAPE_UUID,
                block_size: BLOCK_SIZE,
                covered_prefix_tape_file_count: 2,
                object_row_count: 1,
            },
            second_scope: None,
            fail_after_rows_on_second_visit: None,
            rows: Vec::new(),
            visits: 0,
        };
        let mut objects = Vec::new();
        let error = recover_terminal_inventory_from_bot_with_authority(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            &mut authority,
            |object| {
                objects.push(object.clone());
                Ok(())
            },
        )
        .expect_err("authority must cover every measured Object in its prefix");

        assert!(matches!(
            error,
            BotStructuralRecoveryError::ObjectAuthority { .. }
        ));
        assert!(objects.is_empty());
        assert_eq!(authority.visits, 1);
    }

    #[test]
    fn bot_recovery_rejects_nul_in_authority_object_id_before_emitting_objects() {
        let records = vec![
            Record::Block(bot_bootstrap()),
            Record::Filemark,
            Record::Block(vec![0x10; BLOCK_SIZE as usize]),
            Record::Filemark,
        ];
        let mut source = RecordingSource::new(records);
        let mut authority = TestBotAuthority {
            scope: BotObjectRecoveryAuthorityScope {
                tape_uuid: TAPE_UUID,
                block_size: BLOCK_SIZE,
                covered_prefix_tape_file_count: 2,
                object_row_count: 1,
            },
            second_scope: None,
            fail_after_rows_on_second_visit: None,
            rows: vec![BotObjectRecoveryAuthorityRow {
                tape_file_number: 1,
                stored_block_count: 1,
                object_id: b"nul\0object".to_vec(),
            }],
            visits: 0,
        };
        let mut objects = Vec::new();
        let error = recover_terminal_inventory_from_bot_with_authority(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            &mut authority,
            |object| {
                objects.push(object.clone());
                Ok(())
            },
        )
        .expect_err("NUL-bearing checkpoint identity must fail closed");

        assert!(matches!(
            error,
            BotStructuralRecoveryError::ConflictingObjectAuthority {
                tape_file_number: 1,
                ..
            }
        ));
        assert!(objects.is_empty());
        assert_eq!(authority.visits, 1);
    }

    #[test]
    fn bot_recovery_discards_staged_identities_when_second_scope_changes() {
        let records = vec![
            Record::Block(bot_bootstrap()),
            Record::Filemark,
            Record::Block(vec![0x10; BLOCK_SIZE as usize]),
            Record::Filemark,
        ];
        let mut source = RecordingSource::new(records);
        let scope = BotObjectRecoveryAuthorityScope {
            tape_uuid: TAPE_UUID,
            block_size: BLOCK_SIZE,
            covered_prefix_tape_file_count: 2,
            object_row_count: 1,
        };
        let mut authority = TestBotAuthority {
            scope,
            second_scope: Some(BotObjectRecoveryAuthorityScope {
                object_row_count: 2,
                ..scope
            }),
            fail_after_rows_on_second_visit: None,
            rows: vec![BotObjectRecoveryAuthorityRow {
                tape_file_number: 1,
                stored_block_count: 1,
                object_id: b"provisional-scope-object".to_vec(),
            }],
            visits: 0,
        };
        let mut objects = Vec::new();
        let error = recover_terminal_inventory_from_bot_with_authority(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            &mut authority,
            |object| {
                objects.push(object.clone());
                Ok(())
            },
        )
        .expect_err("changed second scope must discard provisional identities");

        assert!(matches!(
            error,
            BotStructuralRecoveryError::ObjectAuthority { message }
                if message.contains("changed between validation and staging passes")
        ));
        assert!(objects.is_empty());
        assert_eq!(authority.visits, 2);
    }

    #[test]
    fn bot_recovery_discards_staged_identities_after_late_second_replay_failure() {
        let records = vec![
            Record::Block(bot_bootstrap()),
            Record::Filemark,
            Record::Block(vec![0x10; BLOCK_SIZE as usize]),
            Record::Filemark,
        ];
        let mut source = RecordingSource::new(records);
        let mut authority = TestBotAuthority {
            scope: BotObjectRecoveryAuthorityScope {
                tape_uuid: TAPE_UUID,
                block_size: BLOCK_SIZE,
                covered_prefix_tape_file_count: 2,
                object_row_count: 1,
            },
            second_scope: None,
            fail_after_rows_on_second_visit: Some(1),
            rows: vec![BotObjectRecoveryAuthorityRow {
                tape_file_number: 1,
                stored_block_count: 1,
                object_id: b"provisional-replay-object".to_vec(),
            }],
            visits: 0,
        };
        let mut objects = Vec::new();
        let error = recover_terminal_inventory_from_bot_with_authority(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            &mut authority,
            |object| {
                objects.push(object.clone());
                Ok(())
            },
        )
        .expect_err("late replay failure must discard provisional identities");

        assert!(matches!(
            error,
            BotStructuralRecoveryError::ObjectAuthority { message }
                if message.contains("injected late second authority replay failure")
        ));
        assert!(objects.is_empty());
        assert_eq!(authority.visits, 2);
    }

    #[test]
    fn bot_recovery_emits_exact_unknown_and_torn_states_after_authority_commits() {
        let records = vec![
            Record::Block(bot_bootstrap()),
            Record::Filemark,
            Record::Block(vec![0x10; BLOCK_SIZE as usize]),
            Record::Filemark,
            Record::Block(vec![0x20; BLOCK_SIZE as usize]),
            Record::Filemark,
            Record::Block(vec![0x30; BLOCK_SIZE as usize]),
        ];
        let mut source = RecordingSource::new(records);
        let mut authority = TestBotAuthority {
            scope: BotObjectRecoveryAuthorityScope {
                tape_uuid: TAPE_UUID,
                block_size: BLOCK_SIZE,
                covered_prefix_tape_file_count: 2,
                object_row_count: 1,
            },
            second_scope: None,
            fail_after_rows_on_second_visit: None,
            rows: vec![BotObjectRecoveryAuthorityRow {
                tape_file_number: 1,
                stored_block_count: 1,
                object_id: b"committed-object".to_vec(),
            }],
            visits: 0,
        };
        let mut objects = Vec::new();
        let summary = recover_terminal_inventory_from_bot_with_authority(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            &mut authority,
            |object| {
                objects.push(object.clone());
                Ok(())
            },
        )
        .expect("exact authority and physical tail must classify successfully");

        assert_eq!(authority.visits, 2);
        assert_eq!(summary.complete_object_count, 2);
        assert_eq!(summary.recovered_object_count, 1);
        assert_eq!(summary.unknown_object_count, 1);
        assert_eq!(summary.incomplete_object_count, 1);
        assert_eq!(objects.len(), 3);
        assert_eq!(objects[0].state, BotRecoveredObjectState::Recovered);
        assert_eq!(
            objects[0].object_id.as_deref(),
            Some(b"committed-object".as_slice())
        );
        assert_eq!(objects[1].state, BotRecoveredObjectState::Unknown);
        assert!(objects[1].object_id.is_none());
        assert_eq!(objects[2].state, BotRecoveredObjectState::Incomplete);
        assert!(objects[2].object_id.is_none());
    }

    #[test]
    fn bot_recovery_emits_torn_object_as_incomplete() {
        let records = vec![
            Record::Block(bot_bootstrap()),
            Record::Filemark,
            Record::Block(vec![0xA7; BLOCK_SIZE as usize]),
        ];
        let mut source = RecordingSource::new(records);
        let mut objects = Vec::new();
        let summary =
            recover_terminal_inventory_from_bot(&mut source, &TAPE_UUID, BLOCK_SIZE, |object| {
                objects.push(object.clone());
                Ok(())
            })
            .expect("BOT recovery with torn Object");
        assert_eq!(summary.complete_object_count, 0);
        assert_eq!(summary.incomplete_object_count, 1);
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].tape_file_number, 1);
        assert_eq!(objects[0].state, BotRecoveredObjectState::Incomplete);
        assert_eq!(objects[0].stored_block_count, 0);
    }

    #[test]
    fn bot_recovery_announces_fallback_and_honors_pre_scan_abort() {
        let records = vec![Record::Block(bot_bootstrap()), Record::Filemark];
        let mut source = RecordingSource::new(records);
        let mut events = Vec::new();
        let error = recover_terminal_inventory_from_bot_controlled(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            |event| {
                events.push(*event);
                ScanWalkControl::Abort
            },
            |_| Ok(()),
        )
        .expect_err("start-boundary cancellation must stop recovery");

        assert_eq!(events, vec![BotStructuralRecoveryEvent::Started]);
        assert!(source.read_lbas.is_empty());
        assert!(matches!(
            error,
            BotStructuralRecoveryError::Aborted {
                last_tape_file_number: None,
                structural_candidate_count: 0,
                position: None,
                elapsed_millis: 0,
            }
        ));
    }

    #[test]
    fn bot_recovery_emits_progress_before_each_between_files_decision() {
        let records = vec![
            Record::Block(bot_bootstrap()),
            Record::Filemark,
            Record::Block(vec![0x10; BLOCK_SIZE as usize]),
            Record::Filemark,
        ];
        let mut source = RecordingSource::new(records);
        let mut events = Vec::new();
        let error = recover_terminal_inventory_from_bot_controlled(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            |event| {
                events.push(*event);
                match event {
                    BotStructuralRecoveryEvent::Started => ScanWalkControl::Continue,
                    BotStructuralRecoveryEvent::Progress(_) => ScanWalkControl::Abort,
                }
            },
            |_| Ok(()),
        )
        .expect_err("file-boundary cancellation must stop recovery");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0], BotStructuralRecoveryEvent::Started);
        let BotStructuralRecoveryEvent::Progress(progress) = events[1] else {
            panic!("second event must report the completed BOT file")
        };
        assert_eq!(progress.tape_file_number, 0);
        assert_eq!(progress.structural_candidate_count, 1);
        assert!(!source.read_lbas.contains(&2));
        assert!(matches!(
            error,
            BotStructuralRecoveryError::Aborted {
                last_tape_file_number: Some(0),
                structural_candidate_count: 1,
                position: Some(PhysicalPositionHint {
                    partition: 0,
                    lba: 2
                }),
                ..
            }
        ));
    }

    #[test]
    fn bot_recovery_rejects_a_readable_bootstrap_from_another_tape_identity() {
        let records = vec![Record::Block(bot_bootstrap()), Record::Filemark];
        let mut source = RecordingSource::new(records);
        let error =
            recover_terminal_inventory_from_bot(&mut source, &[0x99; 16], BLOCK_SIZE, |_| Ok(()))
                .expect_err("a readable foreign BOT Bootstrap must fence recovery");
        assert!(matches!(
            error,
            BotStructuralRecoveryError::TapeIdentityMismatch
        ));
    }

    #[test]
    fn bot_bootstrap_probe_preserves_absence_and_medium_damage() {
        let records = vec![Record::Block(bot_bootstrap()), Record::Filemark];
        let mut sources = [
            RecordingSource::new(records).with_read_fault(0, TestReadFault::Medium),
            RecordingSource::new(Vec::new()),
        ];

        for source in &mut sources {
            reject_readable_foreign_bot_bootstrap(source, &TAPE_UUID, BLOCK_SIZE)
                .expect("absence or medium damage does not prove a foreign Bootstrap");
        }
    }

    #[test]
    fn bot_bootstrap_probe_propagates_positioning_and_non_medium_read_failures() {
        for fault in [
            TestReadFault::Medium,
            TestReadFault::Hardware,
            TestReadFault::Transport,
        ] {
            let records = vec![Record::Block(bot_bootstrap()), Record::Filemark];
            let mut configure_source =
                RecordingSource::new(records.clone()).with_configure_fault(fault);
            let error = reject_readable_foreign_bot_bootstrap(
                &mut configure_source,
                &TAPE_UUID,
                BLOCK_SIZE,
            )
            .expect_err("non-medium configure failure must abort BOT recovery");
            assert!(matches!(
                error,
                BotStructuralRecoveryError::Scan { message }
                    if message.contains("configure fixed block size")
            ));

            let mut locate_source =
                RecordingSource::new(records.clone()).with_locate_fault(0, fault);
            let error =
                reject_readable_foreign_bot_bootstrap(&mut locate_source, &TAPE_UUID, BLOCK_SIZE)
                    .expect_err("non-medium locate failure must abort BOT recovery");
            assert!(matches!(
                error,
                BotStructuralRecoveryError::Scan { message }
                    if message.contains("locate BOT Bootstrap")
            ));
        }

        for fault in [
            TestReadFault::DeferredFixedMedium,
            TestReadFault::DeferredDescriptorMedium,
            TestReadFault::Hardware,
            TestReadFault::Transport,
        ] {
            let records = vec![Record::Block(bot_bootstrap()), Record::Filemark];
            let mut read_source = RecordingSource::new(records).with_read_fault(0, fault);
            let error =
                reject_readable_foreign_bot_bootstrap(&mut read_source, &TAPE_UUID, BLOCK_SIZE)
                    .expect_err("non-medium read failure must abort BOT recovery");
            assert!(matches!(
                error,
                BotStructuralRecoveryError::Scan { message }
                    if message.contains("read BOT Bootstrap")
            ));
        }
    }

    #[test]
    fn full_verify_bot_head_faults_only_degrade_medium_errors() {
        let fixture = triple_fixture();
        let mut medium_source =
            RecordingSource::new(fixture.records.clone()).with_read_fault(0, TestReadFault::Medium);
        let medium_outcome = verify_terminal_index_full(&mut medium_source, &TAPE_UUID, BLOCK_SIZE)
            .expect("BOT medium damage remains a typed recovery outcome");
        assert!(matches!(
            medium_outcome,
            TerminalIndexVerificationOutcome::RecoveryRequired(_)
        ));

        for fault in [
            TestReadFault::DeferredFixedMedium,
            TestReadFault::DeferredDescriptorMedium,
            TestReadFault::Hardware,
            TestReadFault::Transport,
        ] {
            let mut source =
                RecordingSource::new(fixture.records.clone()).with_read_fault(0, fault);
            let error = verify_terminal_index_full(&mut source, &TAPE_UUID, BLOCK_SIZE)
                .expect_err("non-medium BOT source failure must abort full verification");
            assert!(matches!(
                error,
                TerminalIndexVerificationError::PrefixWalk { .. }
            ));
        }
    }

    #[test]
    fn invalid_c_falls_back_to_b_with_typed_degraded_evidence() {
        let mut fixture = triple_fixture();
        corrupt_replica_payload(&mut fixture, 3);
        let mut source = RecordingSource::new(fixture.records);
        let outcome = read_terminal_index_inventory_summary(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("fallback inventory");
        let TerminalInventoryOutcome::Inventory(selection) = outcome else {
            panic!("B must survive")
        };
        assert_eq!(selection.selected_replica_ordinal, 2);
        assert!(selection.is_degraded());
        assert!(matches!(
            &selection.replicas[2],
            TerminalReplicaEvidence::Invalid(TerminalReplicaFailure {
                kind: TerminalReplicaFailureKind::PayloadInvalid,
                ..
            })
        ));
        for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
            let payload_lba = fixture
                .layout
                .replica(ordinal)
                .expect("replica")
                .planned_start_lba
                .checked_add(1)
                .expect("payload LBA");
            assert_eq!(
                source
                    .read_lbas
                    .iter()
                    .filter(|lba| **lba == payload_lba)
                    .count(),
                usize::from(ordinal >= 2),
                "fallback must try C then B exactly once and never stream A"
            );
        }
    }

    #[test]
    fn medium_error_in_c_payload_falls_back_to_b() {
        let fixture = triple_fixture();
        let c_payload_lba = fixture
            .layout
            .replica(3)
            .expect("replica C")
            .planned_start_lba
            .checked_add(1)
            .expect("replica C payload LBA");
        let mut source = RecordingSource::new(fixture.records)
            .with_read_fault(c_payload_lba, TestReadFault::Medium);

        let outcome = read_terminal_index_inventory_summary(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("a medium error invalidates only the affected candidate");
        let TerminalInventoryOutcome::Inventory(selection) = outcome else {
            panic!("replica B must survive a medium error in replica C")
        };
        assert_eq!(selection.selected_replica_ordinal, 2);
        assert!(matches!(
            &selection.replicas[2],
            TerminalReplicaEvidence::Invalid(TerminalReplicaFailure {
                kind: TerminalReplicaFailureKind::PayloadInvalid,
                ..
            })
        ));
    }

    #[test]
    fn non_medium_payload_errors_abort_without_fallback() {
        for fault in [
            TestReadFault::DeferredFixedMedium,
            TestReadFault::DeferredDescriptorMedium,
            TestReadFault::Hardware,
            TestReadFault::Transport,
        ] {
            let fixture = triple_fixture();
            let c_payload_lba = fixture
                .layout
                .replica(3)
                .expect("replica C")
                .planned_start_lba
                .checked_add(1)
                .expect("replica C payload LBA");
            let b_payload_lba = fixture
                .layout
                .replica(2)
                .expect("replica B")
                .planned_start_lba
                .checked_add(1)
                .expect("replica B payload LBA");
            let mut source =
                RecordingSource::new(fixture.records).with_read_fault(c_payload_lba, fault);

            let error = read_terminal_index_inventory_summary(&mut source, &TAPE_UUID, BLOCK_SIZE)
                .expect_err("a non-medium source error must abort discovery");
            assert!(matches!(
                error,
                TerminalInventoryReadError::Source {
                    operation: "read terminal replica payload",
                    ..
                }
            ));
            assert!(
                !source.read_lbas.contains(&b_payload_lba),
                "fallback must not continue after a non-medium source error"
            );
        }
    }

    #[test]
    fn terminal_footer_probe_distinguishes_medium_and_transport_errors() {
        let fixture = triple_fixture();
        let c_footer_lba = fixture
            .layout
            .replica(3)
            .expect("replica C")
            .planned_start_lba
            .checked_add(
                fixture
                    .layout
                    .replica(3)
                    .expect("replica C")
                    .record_count
                    .checked_sub(1)
                    .expect("replica C footer offset"),
            )
            .expect("replica C footer LBA");

        let mut medium_source = RecordingSource::new(fixture.records.clone())
            .with_read_fault(c_footer_lba, TestReadFault::Medium);
        let medium_outcome =
            read_terminal_index_inventory_summary(&mut medium_source, &TAPE_UUID, BLOCK_SIZE)
                .expect("a medium error during footer probing remains candidate damage");
        assert!(matches!(
            medium_outcome,
            TerminalInventoryOutcome::Inventory(_)
        ));

        let mut transport_source = RecordingSource::new(fixture.records)
            .with_read_fault(c_footer_lba, TestReadFault::Transport);
        let error =
            read_terminal_index_inventory_summary(&mut transport_source, &TAPE_UUID, BLOCK_SIZE)
                .expect_err("transport failure during footer probing must abort");
        assert!(matches!(
            error,
            TerminalInventoryReadError::Source {
                operation: "read terminal footer candidate",
                ..
            }
        ));
    }

    #[test]
    fn streamed_fallback_rejects_c_and_commits_b_attempt_without_body_replay() {
        let mut fixture = triple_fixture();
        corrupt_replica_payload(&mut fixture, 3);
        let mut source = RecordingSource::new(fixture.records);
        let mut events = Vec::new();
        let outcome =
            read_terminal_index_inventory_streamed(&mut source, &TAPE_UUID, BLOCK_SIZE, |event| {
                events.push(event);
                Ok(())
            })
            .expect("streamed fallback inventory");
        let TerminalInventoryOutcome::Inventory(selection) = outcome else {
            panic!("B must survive")
        };

        assert_eq!(selection.selected_replica_ordinal, 2);
        assert_eq!(selection.selected_attempt_id, 2);
        assert!(matches!(
            events.first(),
            Some(TerminalInventoryStreamEvent::ReplicaAttemptStarted {
                attempt_id: 1,
                replica_ordinal: 3,
            })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalInventoryStreamEvent::ReplicaAttemptRejected {
                attempt_id: 1,
                replica_ordinal: 3,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalInventoryStreamEvent::StructuralEntry {
                attempt_id: 2,
                replica_ordinal: 2,
                ..
            }
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            TerminalInventoryStreamEvent::ReplicaAttemptStarted {
                replica_ordinal: 1,
                ..
            }
        )));
        for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
            let payload_lba = fixture
                .layout
                .replica(ordinal)
                .expect("replica")
                .planned_start_lba
                .checked_add(1)
                .expect("payload LBA");
            assert_eq!(
                source
                    .read_lbas
                    .iter()
                    .filter(|lba| **lba == payload_lba)
                    .count(),
                usize::from(ordinal >= 2),
                "streamed fallback must read C and B once and never read A"
            );
        }
    }

    #[test]
    fn invalid_c_and_b_fall_back_to_a() {
        let mut fixture = triple_fixture();
        corrupt_replica_payload(&mut fixture, 3);
        corrupt_replica_payload(&mut fixture, 2);
        let mut source = RecordingSource::new(fixture.records);
        let outcome = read_terminal_index_inventory_summary(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect("A fallback inventory");
        let TerminalInventoryOutcome::Inventory(selection) = outcome else {
            panic!("A must survive")
        };
        assert_eq!(selection.selected_replica_ordinal, 1);
        assert!(selection.is_degraded());
        for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
            let payload_lba = fixture
                .layout
                .replica(ordinal)
                .expect("replica")
                .planned_start_lba
                .checked_add(1)
                .expect("payload LBA");
            assert_eq!(
                source
                    .read_lbas
                    .iter()
                    .filter(|lba| **lba == payload_lba)
                    .count(),
                1,
                "fallback must try C, B, and A exactly once"
            );
        }
    }

    #[test]
    fn conflicting_payload_valid_survivors_fail_without_selecting_newer() {
        let mut fixture = triple_fixture();
        replace_replica_edition(&mut fixture, 2, [0x99; 16]);
        let mut source = RecordingSource::new(fixture.records);
        let error = read_terminal_index_inventory(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect_err("two payload-valid editions must fail closed");
        assert!(matches!(
            error,
            TerminalInventoryReadError::TerminalIndexReplicaConflict { count: 2 }
        ));
    }

    #[test]
    fn fast_inventory_rejects_cross_layout_survivors_before_selecting_one() {
        let mut fixture = triple_fixture();
        let primary_b = fixture.layout.replica(2).expect("primary B component");
        let primary_c = fixture.layout.replica(3).expect("primary C component");
        let replica_records = primary_c.record_count;
        let primary_separation_records = fixture
            .layout
            .separation(1)
            .expect("primary AB component")
            .record_count;
        let alternate_separation_records = replica_records
            .checked_add(
                primary_separation_records
                    .checked_mul(2)
                    .expect("doubled hostile separation count"),
            )
            .and_then(|records| records.checked_add(2))
            .expect("hostile separation count");
        let alternate_first_start = fixture
            .layout
            .replica(1)
            .expect("primary A")
            .planned_start_lba;
        let alternate_layout = TerminalTailLayout::new(
            0,
            BLOCK_SIZE,
            1,
            alternate_first_start,
            replica_records,
            alternate_separation_records,
        )
        .expect("alternate hostile layout");
        assert_eq!(
            alternate_layout
                .replica(2)
                .expect("alternate B")
                .planned_start_lba,
            primary_c.planned_start_lba,
            "alternate B must occupy the same physical LBA as primary C"
        );
        assert!(alternate_layout.expected_eod_lba > fixture.layout.expected_eod_lba);

        let mut rows = BotOnlyRows;
        let alternate_plan = plan_tape_index_replica(edition_plan(alternate_layout, [0x99; 16]), 2)
            .expect("alternate B plan");
        let mut alternate_blocks = Vec::new();
        write_tape_index_replica(
            &alternate_plan,
            TapeIndexReplicaObservation {
                tape_file_number: alternate_plan.component.planned_tape_file_number,
                start_lba: alternate_plan.component.planned_start_lba,
                record_count: alternate_plan.component.record_count,
            },
            &mut rows,
            |block| {
                alternate_blocks.push(block.to_vec());
                Ok(())
            },
        )
        .expect("encode alternate B survivor");
        for (offset, block) in alternate_blocks.into_iter().enumerate() {
            let lba = primary_c
                .planned_start_lba
                .checked_add(u64::try_from(offset).expect("alternate offset"))
                .expect("alternate block LBA");
            fixture.records[usize::try_from(lba).expect("alternate block index")] =
                Record::Block(block);
        }

        let primary_b_payload_lba = primary_b
            .planned_start_lba
            .checked_add(1)
            .expect("primary B payload LBA");
        let alternate_b_payload_lba = primary_c
            .planned_start_lba
            .checked_add(1)
            .expect("alternate B payload LBA");
        let mut source = RecordingSource::new(fixture.records);
        let error = read_terminal_index_inventory_summary(&mut source, &TAPE_UUID, BLOCK_SIZE)
            .expect_err("two physically discovered survivor layouts must fail closed");
        assert!(matches!(
            error,
            TerminalInventoryReadError::TerminalIndexReplicaConflict { count: 2 }
        ));
        assert!(source.read_lbas.contains(&primary_b_payload_lba));
        assert!(source.read_lbas.contains(&alternate_b_payload_lba));
    }

    #[test]
    fn fast_inventory_rejects_4096_before_media_motion() {
        struct EmptyLegacySource {
            configured: Option<u32>,
        }

        impl RawTapeSource for EmptyLegacySource {
            fn configure_fixed_block_size(&mut self, block_size: u32) -> Result<(), ParityError> {
                self.configured = Some(block_size);
                Ok(())
            }

            fn locate_physical(&mut self, _hint: PhysicalPositionHint) -> Result<(), ParityError> {
                Ok(())
            }

            fn locate_end_of_data(&mut self) -> Result<PhysicalPositionHint, ParityError> {
                Ok(PhysicalPositionHint::new(0))
            }

            fn space_filemarks(
                &mut self,
                _count: i64,
            ) -> Result<crate::SpaceFilemarksOutcome, ParityError> {
                Ok(crate::SpaceFilemarksOutcome {
                    filemarks_spaced: 0,
                    position_after: PhysicalPositionHint::new(0),
                    hit_end_of_data: true,
                })
            }

            fn read_record(&mut self, _buf: &mut [u8]) -> Result<RawReadOutcome, ParityError> {
                Ok(RawReadOutcome::EndOfData {
                    position_after: PhysicalPositionHint::new(0),
                })
            }

            fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
                Ok(PhysicalPositionHint::new(0))
            }
        }

        let mut source = EmptyLegacySource { configured: None };
        let error = read_terminal_index_inventory_summary(&mut source, &TAPE_UUID, 4096)
            .expect_err("4096-byte geometry is not a terminal-tail profile");
        assert!(matches!(
            error,
            TerminalInventoryReadError::BlockSize(
                crate::terminal_tail::TerminalTailLayoutError::UnsupportedBlockSize {
                    block_size: 4096
                }
            )
        ));
        assert_eq!(source.configured, None);
    }

    #[test]
    fn all_invalid_requires_bot_recovery_instead_of_empty_inventory() {
        let mut fixture = triple_fixture();
        for ordinal in 1..=TERMINAL_INDEX_REPLICA_COUNT {
            corrupt_replica_payload(&mut fixture, ordinal);
        }
        let mut source = RecordingSource::new(fixture.records);
        let mut visited = false;
        let outcome = read_terminal_index_inventory(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            |_| {
                visited = true;
                Ok(())
            },
            |_| Ok(()),
        )
        .expect("typed recovery outcome");
        let TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery) = outcome else {
            panic!("all invalid members cannot return inventory")
        };
        assert_eq!(
            recovery.reason,
            BotStructuralRecoveryReason::AllMembersInvalid
        );
        assert!(!visited);
        assert!(recovery
            .replicas
            .iter()
            .all(|evidence| matches!(evidence, TerminalReplicaEvidence::Invalid(_))));
    }

    #[test]
    fn absent_terminal_footer_requires_bot_recovery_after_bounded_tail_probe() {
        let records = vec![
            Record::Block(vec![0xA5; BLOCK_SIZE as usize]),
            Record::Filemark,
        ];
        let mut source = RecordingSource::new(records);
        let outcome = read_terminal_index_inventory(
            &mut source,
            &TAPE_UUID,
            BLOCK_SIZE,
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("typed recovery outcome");
        let TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery) = outcome else {
            panic!("missing terminal footer cannot return inventory")
        };
        assert_eq!(
            recovery.reason,
            BotStructuralRecoveryReason::NoUsableTerminalLayout
        );
        assert!(source.read_lbas.len() <= TERMINAL_TAIL_COMPONENT_COUNT);
    }
}
