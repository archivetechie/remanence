//! Read-only live and hermetic terminal triple-index verification driver.
//!
//! The driver deliberately injects read failures above the transport instead
//! of rewriting media.  Its healthy pass discovers the immutable A/gap/B/gap/C
//! layout from EOD; the selected damage plan then proves C-to-B-to-A fallback
//! (or the typed all-invalid recovery handoff) against the same loaded tape.
//! The optional full pass performs the production structural walk and validates
//! every terminal component body, footer, placement, and filemark.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, ValueEnum};
use remanence_library::{DriveHandle, LinuxSgTransport, TapeIoError};
use remanence_parity::{
    encode_tape_index_bootstrap_footer, encode_tape_index_replica_header, plan_index_separation,
    plan_tape_index_edition, plan_tape_index_replica, read_terminal_index_inventory,
    reconcile_terminal_tail_next, recover_terminal_inventory_from_bot, verify_terminal_index_full,
    write_terminal_tail_step, BotStructuralRecoveryReason, DriveHandleRawSource,
    IndexSeparationDescriptor, ParityError, PhysicalPositionHint, RawReadOutcome, RawTapeSink,
    RawTapeSource, RawWriteOutcome, SpaceFilemarksOutcome, TapeIndexReplicaMapEntry,
    TapeIndexReplicaObjectRow, TapeIndexReplicaObservation, TapeIndexReplicaRecordSource,
    TerminalComponentCommit, TerminalComponentReconcileEvidence, TerminalInventoryOutcome,
    TerminalInventoryReadError, TerminalReplicaEvidence, TerminalSeparationEvidence,
    TerminalTailAuthority, TerminalTailComponentPlan, TerminalTailProgress,
    TerminalTailStepOutcome, TerminalTripleWritePlan,
};
use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const REPORT_SCHEMA: &str = "rem.tape.terminal-index-drill.v1";

fn serialize_u64_decimal<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn serialize_u64_decimal_vec<S>(values: &[u64], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    values
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .serialize(serializer)
}

/// One read-side damage arrangement for the five-file terminal tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TerminalIndexDamagePlan {
    /// No injected damage; require healthy selection of C.
    None,
    /// Make A's header unreadable.
    A,
    /// Make B's header unreadable.
    B,
    /// Make C's header unreadable.
    C,
    /// Make A and B unreadable.
    Ab,
    /// Make A and C unreadable.
    Ac,
    /// Make B and C unreadable.
    Bc,
    /// Make all three members unreadable and require BOT recovery handoff.
    Abc,
    /// Replace A's envelope with a locally valid but conflicting edition.
    Disagreement,
    /// Make the AB separation header unreadable during full verification.
    GapAbHeader,
    /// Make the AB separation footer unreadable during full verification.
    GapAbFooter,
    /// Make the BC separation header unreadable during full verification.
    GapBcHeader,
    /// Make the BC separation footer unreadable during full verification.
    GapBcFooter,
}

/// Read-only reconciliation outcomes that the TIX drill may force above the
/// transport before invoking the production terminal-tail decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum TerminalReconcileDrill {
    /// Make the next component readable at its proved start only as a torn WORM tail.
    TornWorm,
    /// Make the next component's immutable start unprovable.
    UnprovedStart,
}

impl TerminalReconcileDrill {
    const fn requested_evidence(self) -> &'static str {
        match self {
            Self::TornWorm => "torn_worm",
            Self::UnprovedStart => "unproved",
        }
    }

    const fn expected_evidence(self) -> TerminalComponentReconcileEvidence {
        match self {
            Self::TornWorm => TerminalComponentReconcileEvidence::TornWorm,
            Self::UnprovedStart => TerminalComponentReconcileEvidence::Unproved,
        }
    }

    const fn injection_kind(self) -> &'static str {
        match self {
            Self::TornWorm => "read_side_transport_error",
            Self::UnprovedStart => "read_side_unproved_start",
        }
    }
}

impl TerminalIndexDamagePlan {
    fn ordinals(self) -> &'static [u16] {
        match self {
            Self::None => &[],
            Self::A => &[1],
            Self::B => &[2],
            Self::C => &[3],
            Self::Ab => &[1, 2],
            Self::Ac => &[1, 3],
            Self::Bc => &[2, 3],
            Self::Abc => &[1, 2, 3],
            Self::Disagreement
            | Self::GapAbHeader
            | Self::GapAbFooter
            | Self::GapBcHeader
            | Self::GapBcFooter => &[],
        }
    }

    fn expected_selected(self) -> Option<u16> {
        match self {
            Self::None | Self::A | Self::B | Self::Ab => Some(3),
            Self::C | Self::Ac => Some(2),
            Self::Bc => Some(1),
            Self::Abc => None,
            Self::Disagreement
            | Self::GapAbHeader
            | Self::GapAbFooter
            | Self::GapBcHeader
            | Self::GapBcFooter => Some(3),
        }
    }

    fn separation_damage(self) -> Option<(u16, bool)> {
        match self {
            Self::GapAbHeader => Some((1, false)),
            Self::GapAbFooter => Some((1, true)),
            Self::GapBcHeader => Some((2, false)),
            Self::GapBcFooter => Some((2, true)),
            _ => None,
        }
    }

    fn expects_fast_inventory_degraded(self) -> bool {
        self != Self::None && self.separation_damage().is_none()
    }

    fn supports_full_verify(self) -> bool {
        self == Self::None || self.separation_damage().is_some()
    }

    fn injection_mechanism(self) -> &'static str {
        if self == Self::None {
            "none"
        } else if self == Self::Disagreement {
            "read_side_valid_conflicting_envelope_replacement"
        } else if self.separation_damage().is_some() {
            "read_side_fixed_block_replacement_0xd7"
        } else {
            "read_side_transport_error"
        }
    }
}

/// Arguments for `rem-debug tape terminal-index-drill`.
#[derive(Args, Debug)]
pub(crate) struct TerminalIndexDrillArgs {
    /// Explicit SG tape-drive device containing the already-finalized tape.
    #[arg(long, value_name = "/dev/sgN")]
    device: PathBuf,

    /// Exact Remanence tape UUID; a barcode is intentionally not accepted.
    #[arg(long, value_name = "UUID")]
    tape_uuid: Uuid,

    /// Catalog-authoritative fixed tape block size.
    #[arg(long, value_parser = parse_block_size)]
    block_size: u32,

    /// Read-side replica or separation damage arrangement.
    #[arg(long, value_enum, default_value_t = TerminalIndexDamagePlan::None)]
    damage_plan: TerminalIndexDamagePlan,

    /// Force one no-write production reconciliation refusal for system TIX.
    #[arg(long, value_enum)]
    reconcile_outcome: Option<TerminalReconcileDrill>,

    /// Also walk the measured physical prefix and all five terminal files.
    #[arg(long)]
    full_verify: bool,

    /// Destination for the stable JSON report.
    #[arg(long, value_name = "PATH.json")]
    report: PathBuf,
}

impl TerminalIndexDrillArgs {
    pub(crate) fn validate_before_discovery(&self) -> Result<(), String> {
        if self.full_verify && !self.damage_plan.supports_full_verify() {
            return Err(
                "terminal-index --full-verify supports only healthy or gap-damage plans"
                    .to_string(),
            );
        }
        if self.damage_plan.separation_damage().is_some() && !self.full_verify {
            return Err("terminal-index gap-damage plans require --full-verify".to_string());
        }
        if self.reconcile_outcome.is_some() && self.damage_plan != TerminalIndexDamagePlan::None {
            return Err(
                "terminal-index reconciliation injection requires --damage-plan none".to_string(),
            );
        }
        fs::metadata(&self.device).map_err(|error| {
            format!(
                "inspect terminal-index device {}: {error}",
                self.device.display()
            )
        })?;
        if self.report == self.device {
            return Err("terminal-index drill report must not name the tape device".to_string());
        }
        match fs::metadata(&self.report) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!(
                    "terminal-index drill report {} exists and is not a regular file",
                    self.report.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "inspect terminal-index drill report {}: {error}",
                    self.report.display()
                ));
            }
        }
        if let Some(parent) = self
            .report
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            if !parent.is_dir() {
                return Err(format!(
                    "terminal-index drill report parent {} is not a directory",
                    parent.display()
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
struct ReplicaReport {
    ordinal: u16,
    state: &'static str,
    detail: String,
}

#[derive(Clone, Debug, Serialize)]
struct FastInventoryReport {
    outcome: &'static str,
    selected_replica_ordinal: Option<u16>,
    degraded: bool,
    /// Stable typed refusal, when inventory deliberately selected no member.
    refusal_error: Option<&'static str>,
    #[serde(serialize_with = "serialize_u64_decimal")]
    conflicting_edition_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    structural_entry_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    object_row_count: u64,
    replicas: Vec<ReplicaReport>,
    #[serde(serialize_with = "serialize_u64_decimal")]
    eod_calls: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    backward_filemark_calls: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    record_reads: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    prefix_record_reads: u64,
}

#[derive(Clone, Debug, Serialize)]
struct FullVerifyReport {
    requested: bool,
    outcome: &'static str,
    complete: bool,
    #[serde(serialize_with = "serialize_u64_decimal")]
    measured_tape_file_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    canonical_prefix_tape_file_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    terminal_components_validated: u64,
    truncation_present: bool,
    separations: Vec<SeparationReport>,
}

#[derive(Clone, Debug, Serialize)]
struct SeparationReport {
    ordinal: u16,
    state: &'static str,
    detail: String,
}

#[derive(Clone, Debug, Serialize)]
struct TerminalComponentReport {
    kind: &'static str,
    ordinal: u16,
    #[serde(serialize_with = "serialize_u64_decimal")]
    tape_file_number: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    start_lba: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    record_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    footer_lba: u64,
}

#[derive(Clone, Debug, Serialize)]
struct TerminalLayoutReport {
    partition: u32,
    #[serde(serialize_with = "serialize_u64_decimal")]
    expected_eod_lba: u64,
    components: Vec<TerminalComponentReport>,
}

#[derive(Clone, Debug, Serialize)]
struct BotRecoveryReport {
    required: bool,
    performed: bool,
    #[serde(serialize_with = "serialize_u64_decimal")]
    structural_entry_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    complete_object_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    recovered_object_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    unknown_object_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    incomplete_object_count: u64,
    #[serde(serialize_with = "serialize_u64_decimal")]
    visited_object_count: u64,
}

/// Stable proof that the production terminal-tail decision refused motion.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct TerminalReconciliationReport {
    requested_evidence: &'static str,
    component: &'static str,
    #[serde(serialize_with = "serialize_u64_decimal")]
    component_start_lba: u64,
    injection_kind: &'static str,
    outcome: &'static str,
    component_motion_attempted: bool,
    terminal_component_admission: &'static str,
    progress_advanced: bool,
}

/// Stable machine-readable result for one read-only live drill leg.
#[derive(Clone, Debug, Serialize)]
struct TerminalIndexDrillReport {
    schema: &'static str,
    report_version: u32,
    execution: &'static str,
    tape_uuid: String,
    block_size_bytes: u32,
    damage_plan: TerminalIndexDamagePlan,
    injection_mechanism: &'static str,
    #[serde(serialize_with = "serialize_u64_decimal_vec")]
    injected_unreadable_lbas: Vec<u64>,
    #[serde(serialize_with = "serialize_u64_decimal_vec")]
    injected_replacement_lbas: Vec<u64>,
    terminal_layout: TerminalLayoutReport,
    fast_inventory: FastInventoryReport,
    bot_recovery: BotRecoveryReport,
    full_verify: FullVerifyReport,
    terminal_reconciliation: Option<TerminalReconciliationReport>,
    capabilities_exercised: Vec<&'static str>,
    expectations_met: bool,
    expectation_failures: Vec<String>,
    success: bool,
}

struct InstrumentedSource<S> {
    inner: S,
    unreadable_lbas: BTreeSet<u64>,
    replacement_records: BTreeMap<u64, Vec<u8>>,
    unproved_lbas: BTreeSet<u64>,
    read_lbas: Vec<u64>,
    eod_calls: u64,
    backward_filemark_calls: u64,
}

impl<S> InstrumentedSource<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            unreadable_lbas: BTreeSet::new(),
            replacement_records: BTreeMap::new(),
            unproved_lbas: BTreeSet::new(),
            read_lbas: Vec::new(),
            eod_calls: 0,
            backward_filemark_calls: 0,
        }
    }

    fn reset_metrics(&mut self) {
        self.read_lbas.clear();
        self.eod_calls = 0;
        self.backward_filemark_calls = 0;
    }
}

impl<S: RawTapeSource> RawTapeSource for InstrumentedSource<S> {
    fn configure_fixed_block_size(
        &mut self,
        block_size: u32,
    ) -> Result<(), remanence_parity::ParityError> {
        self.inner.configure_fixed_block_size(block_size)
    }

    fn locate_physical(
        &mut self,
        hint: PhysicalPositionHint,
    ) -> Result<(), remanence_parity::ParityError> {
        if self.unproved_lbas.contains(&hint.lba) {
            return Err(ParityError::SessionOpen(format!(
                "terminal-index drill injected unproved LBA {}",
                hint.lba
            )));
        }
        self.inner.locate_physical(hint)
    }

    fn locate_end_of_data(
        &mut self,
    ) -> Result<PhysicalPositionHint, remanence_parity::ParityError> {
        self.eod_calls =
            self.eod_calls
                .checked_add(1)
                .ok_or(remanence_parity::ParityError::Invariant(
                    "terminal-index EOD metric overflow",
                ))?;
        self.inner.locate_end_of_data()
    }

    fn space_filemarks(
        &mut self,
        count: i64,
    ) -> Result<SpaceFilemarksOutcome, remanence_parity::ParityError> {
        if count < 0 {
            self.backward_filemark_calls = self.backward_filemark_calls.checked_add(1).ok_or(
                remanence_parity::ParityError::Invariant("terminal-index filemark metric overflow"),
            )?;
        }
        self.inner.space_filemarks(count)
    }

    fn read_record(
        &mut self,
        buf: &mut [u8],
    ) -> Result<RawReadOutcome, remanence_parity::ParityError> {
        let position = self.inner.position()?;
        self.read_lbas.push(position.lba);
        if self.unreadable_lbas.contains(&position.lba) {
            return Err(TapeIoError::OperationFailed(format!(
                "terminal-index drill injected unreadable LBA {}",
                position.lba
            ))
            .into());
        }
        let outcome = self.inner.read_record(buf)?;
        if let Some(replacement) = self.replacement_records.get(&position.lba) {
            if replacement.len() != buf.len() {
                return Err(remanence_parity::ParityError::Invariant(
                    "terminal-index replacement record has the wrong fixed size",
                ));
            }
            buf.copy_from_slice(replacement);
        }
        Ok(outcome)
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, remanence_parity::ParityError> {
        self.inner.position()
    }
}

#[derive(Clone, Default)]
struct CapturedRecords {
    entries: Vec<TapeIndexReplicaMapEntry>,
    rows: Vec<TapeIndexReplicaObjectRow>,
}

impl TapeIndexReplicaRecordSource for CapturedRecords {
    fn visit_structural_entries(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for entry in &self.entries {
            visitor(entry)?;
        }
        Ok(())
    }

    fn visit_object_rows(
        &mut self,
        visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
    ) -> Result<(), ParityError> {
        for row in &self.rows {
            visitor(row)?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct MotionRefusingSink {
    motion_attempts: u64,
}

impl MotionRefusingSink {
    fn reject_motion(&mut self) -> Result<RawWriteOutcome, ParityError> {
        self.motion_attempts =
            self.motion_attempts
                .checked_add(1)
                .ok_or(ParityError::Invariant(
                    "reconciliation drill motion counter overflow",
                ))?;
        Err(ParityError::Invariant(
            "reconciliation drill forbids terminal media motion",
        ))
    }
}

impl RawTapeSink for MotionRefusingSink {
    fn locate_for_overwrite(&mut self, _hint: PhysicalPositionHint) -> Result<(), ParityError> {
        self.reject_motion().map(|_| ())
    }

    fn write_fixed_block(&mut self, _buf: &[u8]) -> Result<RawWriteOutcome, ParityError> {
        self.reject_motion()
    }

    fn write_filemarks(
        &mut self,
        _count: u32,
        _immediate: bool,
    ) -> Result<RawWriteOutcome, ParityError> {
        self.reject_motion()
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, ParityError> {
        self.motion_attempts =
            self.motion_attempts
                .checked_add(1)
                .ok_or(ParityError::Invariant(
                    "reconciliation drill motion counter overflow",
                ))?;
        Err(ParityError::Invariant(
            "reconciliation drill forbids terminal position commands",
        ))
    }
}

struct ReconcileDrillAuthority<'a, S> {
    source: &'a mut InstrumentedSource<S>,
    plan: &'a TerminalTripleWritePlan,
    evidence: Option<TerminalComponentReconcileEvidence>,
    commit_attempted: bool,
}

impl<S: RawTapeSource> TerminalTailAuthority for ReconcileDrillAuthority<'_, S> {
    fn load_progress(&mut self) -> Result<TerminalTailProgress, String> {
        Ok(TerminalTailProgress::BeforeReplicaA)
    }

    fn reconcile_next(
        &mut self,
        progress: TerminalTailProgress,
        component: TerminalTailComponentPlan,
    ) -> Result<TerminalComponentReconcileEvidence, String> {
        if progress != TerminalTailProgress::BeforeReplicaA
            || component != self.plan.edition.descriptor.terminal_layout.components[0]
        {
            return Err("reconciliation drill reached an unexpected component".to_string());
        }
        let evidence = reconcile_terminal_tail_next(self.source, self.plan, progress, false);
        self.evidence = Some(evidence);
        Ok(evidence)
    }

    fn commit_after_barrier(&mut self, _commit: &TerminalComponentCommit) -> Result<(), String> {
        self.commit_attempted = true;
        Err("reconciliation drill must not commit terminal progress".to_string())
    }
}

fn run_terminal_reconciliation_drill<S: RawTapeSource>(
    source: &mut InstrumentedSource<S>,
    records: &mut CapturedRecords,
    edition: remanence_parity::TapeIndexEditionPlan,
    requested: TerminalReconcileDrill,
) -> Result<(TerminalReconciliationReport, Vec<String>), String> {
    let replicas = [
        plan_tape_index_replica(edition.clone(), 1)
            .map_err(|error| format!("plan reconciliation replica A: {error}"))?,
        plan_tape_index_replica(edition.clone(), 2)
            .map_err(|error| format!("plan reconciliation replica B: {error}"))?,
        plan_tape_index_replica(edition.clone(), 3)
            .map_err(|error| format!("plan reconciliation replica C: {error}"))?,
    ];
    let separation = |ordinal| {
        let component = edition
            .descriptor
            .terminal_layout
            .separation(ordinal)
            .map_err(|error| format!("resolve reconciliation gap {ordinal}: {error}"))?;
        let nominal_extent_bytes = component
            .record_count
            .checked_mul(u64::from(edition.descriptor.block_size))
            .ok_or_else(|| "reconciliation drill separation extent overflows".to_string())?;
        plan_index_separation(IndexSeparationDescriptor {
            tape_uuid: edition.descriptor.tape_uuid,
            edition_id: edition.descriptor.edition_id,
            gap_ordinal: ordinal,
            block_size: edition.descriptor.block_size,
            nominal_extent_bytes,
            total_records: component.record_count,
            compression_enabled: edition.descriptor.compression_enabled,
            terminal_layout: edition.descriptor.terminal_layout,
        })
        .map_err(|error| format!("plan reconciliation gap {ordinal}: {error}"))
    };
    let separations = [separation(1)?, separation(2)?];
    let plan = TerminalTripleWritePlan::from_parts(edition, replicas, separations)
        .map_err(|error| format!("plan terminal reconciliation drill: {error}"))?;
    let component = plan.edition.descriptor.terminal_layout.components[0];
    source.unreadable_lbas.clear();
    source.replacement_records.clear();
    source.unproved_lbas.clear();
    match requested {
        TerminalReconcileDrill::TornWorm => {
            source.unreadable_lbas.insert(component.planned_start_lba);
        }
        TerminalReconcileDrill::UnprovedStart => {
            source.unproved_lbas.insert(component.planned_start_lba);
        }
    }

    let mut sink = MotionRefusingSink::default();
    let mut authority = ReconcileDrillAuthority {
        source,
        plan: &plan,
        evidence: None,
        commit_attempted: false,
    };
    let outcome = write_terminal_tail_step(&mut sink, records, &mut authority, &plan)
        .map_err(|error| format!("production terminal reconciliation decision: {error}"))?;
    let observed_evidence = authority.evidence;
    let commit_attempted = authority.commit_attempted;
    let mut failures = Vec::new();
    let recovery_required = matches!(
        outcome,
        TerminalTailStepOutcome::RecoveryRequired {
            progress: TerminalTailProgress::BeforeReplicaA,
            component: observed_component,
            evidence,
        } if observed_component == component && evidence == requested.expected_evidence()
    );
    if !recovery_required {
        failures.push(format!(
            "production decision returned {outcome:?}, expected RecoveryRequired({:?})",
            requested.expected_evidence()
        ));
    }
    if observed_evidence != Some(requested.expected_evidence()) {
        failures.push(format!(
            "production reconciler returned {observed_evidence:?}, expected {:?}",
            requested.expected_evidence()
        ));
    }
    if sink.motion_attempts != 0 {
        failures.push(format!(
            "production decision attempted {} terminal media command(s)",
            sink.motion_attempts
        ));
    }
    if commit_attempted {
        failures.push("production decision attempted to advance durable progress".to_string());
    }
    let refused_without_motion =
        recovery_required && sink.motion_attempts == 0 && !commit_attempted;
    Ok((
        TerminalReconciliationReport {
            requested_evidence: requested.requested_evidence(),
            component: "replica_a",
            component_start_lba: component.planned_start_lba,
            injection_kind: requested.injection_kind(),
            outcome: if recovery_required {
                "recovery_required"
            } else {
                "unexpected"
            },
            component_motion_attempted: sink.motion_attempts != 0,
            terminal_component_admission: if refused_without_motion {
                "refused"
            } else {
                "unexpected"
            },
            // The drill authority never mutates progress. A callback attempt
            // is separately a failed expectation above.
            progress_advanced: false,
        },
        failures,
    ))
}

fn parse_block_size(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|error| format!("invalid terminal-index block size {value:?}: {error}"))?;
    if matches!(parsed, 262_144 | 524_288 | 1_048_576) {
        Ok(parsed)
    } else {
        Err(format!(
            "unsupported terminal-index block size {parsed}; expected 262144, 524288, or 1048576"
        ))
    }
}

fn replica_reports(evidence: &[TerminalReplicaEvidence; 3]) -> Vec<ReplicaReport> {
    evidence
        .iter()
        .enumerate()
        .map(|(index, evidence)| {
            let ordinal = u16::try_from(index + 1).expect("three replica ordinals fit u16");
            match evidence {
                TerminalReplicaEvidence::Valid { .. } => ReplicaReport {
                    ordinal,
                    state: "valid",
                    detail: String::new(),
                },
                TerminalReplicaEvidence::ConsistentEnvelope => ReplicaReport {
                    ordinal,
                    state: "consistent_envelope",
                    detail: String::new(),
                },
                TerminalReplicaEvidence::Invalid(failure) => ReplicaReport {
                    ordinal,
                    state: "invalid",
                    detail: format!("{:?}: {}", failure.kind, failure.detail),
                },
            }
        })
        .collect()
}

fn separation_reports(evidence: &[TerminalSeparationEvidence; 2]) -> Vec<SeparationReport> {
    evidence
        .iter()
        .enumerate()
        .map(|(index, evidence)| {
            let ordinal = u16::try_from(index + 1).expect("two separation ordinals fit u16");
            match evidence {
                TerminalSeparationEvidence::Valid { .. } => SeparationReport {
                    ordinal,
                    state: "valid",
                    detail: String::new(),
                },
                TerminalSeparationEvidence::Invalid { detail } => SeparationReport {
                    ordinal,
                    state: "invalid",
                    detail: detail.clone(),
                },
            }
        })
        .collect()
}

fn inspect_source<S: RawTapeSource>(
    source: S,
    tape_uuid: [u8; 16],
    block_size: u32,
    damage_plan: TerminalIndexDamagePlan,
    reconcile_outcome: Option<TerminalReconcileDrill>,
    full_verify_requested: bool,
    execution: &'static str,
) -> Result<TerminalIndexDrillReport, String> {
    let mut source = InstrumentedSource::new(source);
    let mut captured = CapturedRecords::default();
    let healthy = read_terminal_index_inventory(
        &mut source,
        &tape_uuid,
        block_size,
        |entry| {
            captured.entries.push(entry.clone());
            Ok(())
        },
        |row| {
            captured.rows.push(row.clone());
            Ok(())
        },
    )
    .map_err(|error| format!("discover healthy terminal layout: {error}"))?;
    let TerminalInventoryOutcome::Inventory(healthy) = healthy else {
        return Err("healthy preflight found no usable terminal-index member".to_string());
    };
    let layout = healthy.edition.descriptor.terminal_layout;
    let terminal_layout = TerminalLayoutReport {
        partition: layout.partition,
        expected_eod_lba: layout.expected_eod_lba,
        components: layout
            .components
            .iter()
            .map(|component| TerminalComponentReport {
                kind: match component.kind {
                    remanence_parity::TerminalTailComponentKind::TapeIndexReplica => {
                        "tape_index_replica"
                    }
                    remanence_parity::TerminalTailComponentKind::IndexSeparationExtent => {
                        "index_separation_extent"
                    }
                },
                ordinal: component.ordinal,
                tape_file_number: component.planned_tape_file_number,
                start_lba: component.planned_start_lba,
                record_count: component.record_count,
                footer_lba: component
                    .planned_start_lba
                    .checked_add(component.record_count)
                    .and_then(|exclusive| exclusive.checked_sub(1))
                    .expect("validated terminal component has a footer LBA"),
            })
            .collect(),
    };
    let first_terminal_lba = layout.components[0].planned_start_lba;
    let damage_unreadable_lbas = damage_plan
        .ordinals()
        .iter()
        .map(|ordinal| {
            layout
                .replica(*ordinal)
                .map(|component| component.planned_start_lba)
                .map_err(|error| format!("resolve replica {ordinal} start: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut injected_unreadable_lbas = damage_unreadable_lbas.clone();
    if reconcile_outcome == Some(TerminalReconcileDrill::TornWorm) {
        injected_unreadable_lbas.push(layout.components[0].planned_start_lba);
    }
    let mut injected_replacement_lbas = Vec::new();
    if let Some((ordinal, footer)) = damage_plan.separation_damage() {
        let component = layout
            .separation(ordinal)
            .map_err(|error| format!("resolve separation {ordinal}: {error}"))?;
        let lba = if footer {
            component
                .planned_start_lba
                .checked_add(component.record_count - 1)
                .ok_or_else(|| format!("separation {ordinal} footer LBA overflows"))?
        } else {
            component.planned_start_lba
        };
        source
            .replacement_records
            .insert(lba, vec![0xd7; block_size as usize]);
        injected_replacement_lbas.push(lba);
    }
    source.unreadable_lbas = damage_unreadable_lbas.iter().copied().collect();
    if damage_plan == TerminalIndexDamagePlan::Disagreement {
        let component = layout
            .replica(1)
            .map_err(|error| format!("resolve disagreement replica A: {error}"))?;
        let mut descriptor = healthy.edition.descriptor.clone();
        descriptor.edition_id[0] ^= 0xff;
        descriptor.writer_version = "remanence-tix-conflicting-edition".to_string();
        let mut records = captured.clone();
        let conflicting_edition = plan_tape_index_edition(descriptor, &mut records)
            .map_err(|error| format!("plan conflicting terminal edition: {error}"))?;
        let conflicting = plan_tape_index_replica(conflicting_edition, 1)
            .map_err(|error| format!("plan conflicting replica A: {error}"))?;
        let header = encode_tape_index_replica_header(&conflicting)
            .map_err(|error| format!("encode conflicting replica A header: {error}"))?;
        let header_sha256: [u8; 32] = Sha256::digest(&header).into();
        let observation = TapeIndexReplicaObservation {
            tape_file_number: component.planned_tape_file_number,
            start_lba: component.planned_start_lba,
            record_count: component.record_count,
        };
        let footer = encode_tape_index_bootstrap_footer(&conflicting, header_sha256, observation)
            .map_err(|error| format!("encode conflicting replica A footer: {error}"))?;
        let footer_lba = component
            .planned_start_lba
            .checked_add(component.record_count - 1)
            .ok_or_else(|| "conflicting replica A footer LBA overflows".to_string())?;
        source
            .replacement_records
            .insert(component.planned_start_lba, header);
        source.replacement_records.insert(footer_lba, footer);
        injected_replacement_lbas.extend([component.planned_start_lba, footer_lba]);
    }
    source.reset_metrics();

    let damaged =
        read_terminal_index_inventory(&mut source, &tape_uuid, block_size, |_| Ok(()), |_| Ok(()));
    let prefix_record_reads = source
        .read_lbas
        .iter()
        .filter(|lba| **lba < first_terminal_lba)
        .count();
    let prefix_record_reads = u64::try_from(prefix_record_reads)
        .map_err(|_| "prefix read count exceeds u64::MAX".to_string())?;
    let record_reads = u64::try_from(source.read_lbas.len())
        .map_err(|_| "record read count exceeds u64::MAX".to_string())?;

    let mut expectation_failures = Vec::new();
    if prefix_record_reads != 0 {
        expectation_failures.push(format!(
            "fast inventory read {prefix_record_reads} record(s) before the terminal tail"
        ));
    }
    if source.eod_calls != 1 {
        expectation_failures.push(format!(
            "fast inventory issued {} EOD locates, expected exactly 1",
            source.eod_calls
        ));
    }

    let fast_inventory = match damaged {
        Ok(TerminalInventoryOutcome::Inventory(selection)) => {
            if Some(selection.selected_replica_ordinal) != damage_plan.expected_selected() {
                expectation_failures.push(format!(
                    "selected replica {}, expected {:?} for {:?}",
                    selection.selected_replica_ordinal,
                    damage_plan.expected_selected(),
                    damage_plan
                ));
            }
            let expected_degraded = damage_plan.expects_fast_inventory_degraded();
            if selection.is_degraded() != expected_degraded {
                expectation_failures.push(format!(
                    "degraded={} but expected {expected_degraded} for {:?}",
                    selection.is_degraded(),
                    damage_plan
                ));
            }
            FastInventoryReport {
                outcome: if selection.is_degraded() {
                    "degraded"
                } else {
                    "complete"
                },
                selected_replica_ordinal: Some(selection.selected_replica_ordinal),
                degraded: selection.is_degraded(),
                refusal_error: None,
                conflicting_edition_count: 0,
                structural_entry_count: selection.payload.structural_entry_count,
                object_row_count: selection.payload.object_row_count,
                replicas: replica_reports(&selection.replicas),
                eod_calls: source.eod_calls,
                backward_filemark_calls: source.backward_filemark_calls,
                record_reads,
                prefix_record_reads,
            }
        }
        Ok(TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery)) => {
            if damage_plan != TerminalIndexDamagePlan::Abc
                || recovery.reason != BotStructuralRecoveryReason::AllMembersInvalid
            {
                expectation_failures.push(format!(
                    "unexpected BOT recovery handoff {:?} for {:?}",
                    recovery.reason, damage_plan
                ));
            }
            FastInventoryReport {
                outcome: "bot_structural_recovery_required",
                selected_replica_ordinal: None,
                degraded: true,
                refusal_error: None,
                conflicting_edition_count: 0,
                structural_entry_count: 0,
                object_row_count: 0,
                replicas: replica_reports(&recovery.replicas),
                eod_calls: source.eod_calls,
                backward_filemark_calls: source.backward_filemark_calls,
                record_reads,
                prefix_record_reads,
            }
        }
        Err(TerminalInventoryReadError::TerminalIndexReplicaConflict { count }) => {
            if damage_plan != TerminalIndexDamagePlan::Disagreement {
                return Err(format!(
                    "run damaged terminal inventory: unexpected TerminalIndexReplicaConflict(count={count})"
                ));
            }
            let conflicting_edition_count = u64::try_from(count)
                .map_err(|_| "conflicting edition count exceeds u64::MAX".to_string())?;
            if count != 2 {
                expectation_failures.push(format!(
                    "disagreement reported {count} conflicting editions, expected 2"
                ));
            }
            FastInventoryReport {
                outcome: "refused",
                selected_replica_ordinal: None,
                degraded: true,
                refusal_error: Some("TerminalIndexReplicaConflict"),
                conflicting_edition_count,
                structural_entry_count: 0,
                object_row_count: 0,
                replicas: Vec::new(),
                eod_calls: source.eod_calls,
                backward_filemark_calls: source.backward_filemark_calls,
                record_reads,
                prefix_record_reads,
            }
        }
        Err(error) => return Err(format!("run damaged terminal inventory: {error}")),
    };
    if damage_plan == TerminalIndexDamagePlan::Abc
        && fast_inventory.outcome != "bot_structural_recovery_required"
    {
        expectation_failures.push("all-invalid damage did not require BOT recovery".to_string());
    }
    if damage_plan == TerminalIndexDamagePlan::Disagreement {
        if fast_inventory.selected_replica_ordinal.is_some() {
            expectation_failures
                .push("conflicting editions selected a terminal replica".to_string());
        }
        if fast_inventory.refusal_error != Some("TerminalIndexReplicaConflict") {
            expectation_failures.push(
                "locally valid conflicting editions did not produce TerminalIndexReplicaConflict"
                    .to_string(),
            );
        }
    }

    let mut bot_recovery = BotRecoveryReport {
        required: damage_plan == TerminalIndexDamagePlan::Abc,
        performed: false,
        structural_entry_count: 0,
        complete_object_count: 0,
        recovered_object_count: 0,
        unknown_object_count: 0,
        incomplete_object_count: 0,
        visited_object_count: 0,
    };
    if bot_recovery.required {
        let mut visited_object_count = 0u64;
        let summary =
            recover_terminal_inventory_from_bot(&mut source, &tape_uuid, block_size, |_| {
                visited_object_count = visited_object_count
                    .checked_add(1)
                    .ok_or_else(|| "BOT recovery visitor count overflows u64".to_string())?;
                Ok(())
            })
            .map_err(|error| format!("BOT structural recovery after all-invalid index: {error}"))?;
        bot_recovery = BotRecoveryReport {
            required: true,
            performed: true,
            structural_entry_count: summary.structural_entry_count,
            complete_object_count: summary.complete_object_count,
            recovered_object_count: summary.recovered_object_count,
            unknown_object_count: summary.unknown_object_count,
            incomplete_object_count: summary.incomplete_object_count,
            visited_object_count,
        };
        let classified = summary
            .complete_object_count
            .checked_add(summary.incomplete_object_count)
            .ok_or_else(|| "BOT recovery classified count overflows u64".to_string())?;
        if visited_object_count != classified {
            expectation_failures.push(format!(
                "BOT recovery visited {visited_object_count} Objects but classified {classified}"
            ));
        }
    }
    source.unreadable_lbas.clear();
    if damage_plan.separation_damage().is_none() {
        source.replacement_records.clear();
    }
    let mut full_verify = FullVerifyReport {
        requested: full_verify_requested,
        outcome: "not_requested",
        complete: false,
        measured_tape_file_count: 0,
        canonical_prefix_tape_file_count: healthy
            .edition
            .descriptor
            .scope
            .covered_prefix_tape_file_count,
        terminal_components_validated: 0,
        truncation_present: false,
        separations: Vec::new(),
    };
    if full_verify_requested {
        if !damage_plan.supports_full_verify() {
            return Err("--full-verify supports only healthy or gap-damage plans".to_string());
        }
        let outcome = verify_terminal_index_full(&mut source, &tape_uuid, block_size)
            .map_err(|error| format!("full terminal-index verification: {error}"))?;
        let (verified, outcome_name, complete) = match outcome {
            remanence_parity::TerminalIndexVerificationOutcome::VerifiedComplete(verified) => {
                if damage_plan.separation_damage().is_some() {
                    expectation_failures.push(format!(
                        "gap damage {:?} unexpectedly verified complete",
                        damage_plan
                    ));
                }
                (verified, "verified_complete", true)
            }
            remanence_parity::TerminalIndexVerificationOutcome::VerifiedDegraded(verified) => {
                if damage_plan.separation_damage().is_none() {
                    expectation_failures
                        .push("healthy terminal-index drill verified degraded".to_string());
                }
                (verified, "verified_degraded", false)
            }
            remanence_parity::TerminalIndexVerificationOutcome::RecoveryRequired(_) => {
                return Err(format!(
                    "terminal-index drill {:?} required BOT recovery during full verification",
                    damage_plan
                ));
            }
        };
        if let Some((damaged_ordinal, _)) = damage_plan.separation_damage() {
            let damaged_index = usize::from(damaged_ordinal - 1);
            if !matches!(
                verified.separations[damaged_index],
                TerminalSeparationEvidence::Invalid { .. }
            ) {
                expectation_failures.push(format!(
                    "gap damage {:?} did not produce invalid separation {} evidence",
                    damage_plan, damaged_ordinal
                ));
            }
            let other_index = 1usize - damaged_index;
            if !matches!(
                verified.separations[other_index],
                TerminalSeparationEvidence::Valid { .. }
            ) {
                expectation_failures.push(format!(
                    "gap damage {:?} also invalidated unaffected separation {}",
                    damage_plan,
                    other_index + 1
                ));
            }
        }
        full_verify.measured_tape_file_count = verified.measured_tape_file_count;
        full_verify.canonical_prefix_tape_file_count = verified.verified_prefix_tape_file_count;
        full_verify.terminal_components_validated = verified
            .replicas
            .iter()
            .filter(|evidence| matches!(evidence, TerminalReplicaEvidence::Valid { .. }))
            .count()
            .checked_add(
                verified
                    .separations
                    .iter()
                    .filter(|evidence| matches!(evidence, TerminalSeparationEvidence::Valid { .. }))
                    .count(),
            )
            .and_then(|count| u64::try_from(count).ok())
            .ok_or_else(|| "validated terminal component count overflows u64".to_string())?;
        full_verify.separations = separation_reports(&verified.separations);
        full_verify.outcome = outcome_name;
        full_verify.complete = complete;
    }

    let terminal_reconciliation = if let Some(requested) = reconcile_outcome {
        let (report, failures) = run_terminal_reconciliation_drill(
            &mut source,
            &mut captured,
            healthy.edition.clone(),
            requested,
        )?;
        expectation_failures.extend(failures);
        Some(report)
    } else {
        None
    };

    let mut capabilities_exercised = vec![
        "rem.tape.index.terminal_triple",
        "rem.tape.index.replica_set_integrity",
        "rem.tape.index.fast_inventory",
        "rem.tape.index.eod_locator",
    ];
    if damage_plan.expects_fast_inventory_degraded() {
        capabilities_exercised.push("rem.tape.index.replica_fallback");
    }
    if damage_plan == TerminalIndexDamagePlan::Abc {
        capabilities_exercised.push("rem.tape.index.all_replicas_damaged_recovery");
    }
    if full_verify_requested {
        capabilities_exercised.extend([
            "rem.tape.index.separation_extent",
            "rem.tape.index.full_physical_verify",
        ]);
    }
    if terminal_reconciliation.is_some() {
        capabilities_exercised.push("rem.tape.index.resume_authority");
    }
    let expectations_met = expectation_failures.is_empty();
    Ok(TerminalIndexDrillReport {
        schema: REPORT_SCHEMA,
        report_version: 1,
        execution,
        tape_uuid: Uuid::from_bytes(tape_uuid).to_string(),
        block_size_bytes: block_size,
        damage_plan,
        injection_mechanism: if reconcile_outcome.is_some() {
            "read_side_production_terminal_reconciler"
        } else {
            damage_plan.injection_mechanism()
        },
        injected_unreadable_lbas,
        injected_replacement_lbas,
        terminal_layout,
        fast_inventory,
        bot_recovery,
        full_verify,
        terminal_reconciliation,
        capabilities_exercised,
        expectations_met,
        expectation_failures,
        success: expectations_met,
    })
}

fn write_report(path: &Path, report: &TerminalIndexDrillReport) -> Result<(), String> {
    let mut file = File::create(path)
        .map_err(|error| format!("create terminal-index report {}: {error}", path.display()))?;
    serde_json::to_writer_pretty(&mut file, report).map_err(|error| {
        format!(
            "serialize terminal-index report {}: {error}",
            path.display()
        )
    })?;
    file.write_all(b"\n")
        .map_err(|error| format!("finish terminal-index report {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync terminal-index report {}: {error}", path.display()))
}

/// Run one read-only live SG leg and persist its stable JSON report.
pub(crate) fn run_live(
    args: &TerminalIndexDrillArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    #[cfg(target_os = "linux")]
    let result = (|| {
        let transport = LinuxSgTransport::open_rw(&args.device)
            .map_err(|error| format!("open {}: {error}", args.device.display()))?;
        let mut drive =
            DriveHandle::open_standalone_with_transport(&args.device, Box::new(transport))
                .map_err(|error| {
                    format!("open standalone drive {}: {error}", args.device.display())
                })?;
        let source = DriveHandleRawSource::new(&mut drive);
        inspect_source(
            source,
            *args.tape_uuid.as_bytes(),
            args.block_size,
            args.damage_plan,
            args.reconcile_outcome,
            args.full_verify,
            "live_sg_read_only",
        )
    })();
    #[cfg(not(target_os = "linux"))]
    let result: Result<TerminalIndexDrillReport, String> = Err(format!(
        "terminal-index drill device {} requires Linux SG_IO access",
        args.device.display()
    ));

    let report = match result {
        Ok(report) => report,
        Err(error) => {
            let _ = writeln!(err, "error: terminal-index drill: {error}");
            return ExitCode::from(1);
        }
    };
    if let Err(error) = write_report(&args.report, &report) {
        let _ = writeln!(err, "error: {error}");
        return ExitCode::from(1);
    }
    let _ = writeln!(
        out,
        "terminal-index drill {}: damage={:?} selected={:?}; report {}",
        if report.success { "passed" } else { "failed" },
        report.damage_plan,
        report.fast_inventory.selected_replica_ordinal,
        args.report.display()
    );
    if report.success {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remanence_parity::{
        checked_tape_index_replica_layout, plan_index_separation, plan_tape_index_edition,
        plan_tape_index_replica, write_index_separation, write_tape_index_replica,
        BootstrapPayload, ImageDirectoryRawSource, IndexSeparationDescriptor,
        IndexSeparationObservation, ObjectRecoveryRepresentation, ParityError,
        TapeIndexEditionDescriptor, TapeIndexReplicaCounts, TapeIndexReplicaFileKind,
        TapeIndexReplicaMapEntry, TapeIndexReplicaObjectRow, TapeIndexReplicaObservation,
        TapeIndexReplicaRecordSource, TapeIndexReplicaScope, TerminalTailLayout,
    };

    const BLOCK_SIZE: u32 = 262_144;
    const TAPE_UUID: [u8; 16] = [0x71; 16];

    #[derive(Clone)]
    struct Rows {
        entries: Vec<TapeIndexReplicaMapEntry>,
        rows: Vec<TapeIndexReplicaObjectRow>,
    }

    impl TapeIndexReplicaRecordSource for Rows {
        fn visit_structural_entries(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexReplicaMapEntry) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for entry in &self.entries {
                visitor(entry)?;
            }
            Ok(())
        }

        fn visit_object_rows(
            &mut self,
            visitor: &mut dyn FnMut(&TapeIndexReplicaObjectRow) -> Result<(), ParityError>,
        ) -> Result<(), ParityError> {
            for row in &self.rows {
                visitor(row)?;
            }
            Ok(())
        }
    }

    fn fixture() -> ImageDirectoryRawSource {
        let entries = vec![
            TapeIndexReplicaMapEntry {
                tape_file_number: 0,
                kind: TapeIndexReplicaFileKind::Bootstrap,
                block_count: 1,
                first_parity_data_ordinal: None,
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
            TapeIndexReplicaMapEntry {
                tape_file_number: 1,
                kind: TapeIndexReplicaFileKind::Object,
                block_count: 1,
                first_parity_data_ordinal: Some(0),
                protected_ordinal_start: None,
                protected_ordinal_end_exclusive: None,
                epoch_id: None,
            },
        ];
        let rows = Rows {
            entries,
            rows: vec![TapeIndexReplicaObjectRow {
                tape_file_number: 1,
                stored_block_count: 1,
                object_id: b"terminal-index-drill-object".to_vec(),
                representation: ObjectRecoveryRepresentation::Plaintext {
                    manifest_first_chunk_lba: 0,
                    manifest_size_bytes: 1,
                    manifest_chunk_count: 1,
                    manifest_sha256: [0x91; 32],
                },
            }],
        };
        let counts = TapeIndexReplicaCounts {
            structural_entry_count: 2,
            object_row_count: 1,
        };
        let replica_records = checked_tape_index_replica_layout(BLOCK_SIZE, counts)
            .expect("replica layout")
            .replica_record_count;
        let layout =
            TerminalTailLayout::new(0, BLOCK_SIZE, 2, 4, replica_records, 3).expect("tail layout");
        let mut planning = rows.clone();
        let edition = plan_tape_index_edition(
            TapeIndexEditionDescriptor {
                tape_uuid: TAPE_UUID,
                edition_id: [0x81; 16],
                edition_sequence: 1,
                scope: TapeIndexReplicaScope {
                    covered_prefix_tape_file_count: 2,
                    total_data_ordinals: 1,
                    highest_protected_ordinal: 0,
                },
                counts,
                block_size: BLOCK_SIZE,
                compression_enabled: false,
                writer_version: "terminal-index-drill-test".to_string(),
                write_timestamp: "2026-08-09T00:00:00Z".to_string(),
                terminal_layout: layout,
            },
            &mut planning,
        )
        .expect("edition");

        let mut bot = vec![0; BLOCK_SIZE as usize];
        remanence_parity::bootstrap::write_bootstrap_block(
            &BootstrapPayload {
                scheme: None,
                no_parity_flag: true,
                filemark_map_digest: None,
                tape_uuid: TAPE_UUID,
                written_by_version: "terminal-index-drill-test".to_string(),
                written_at: "2026-08-09T00:00:00Z".to_string(),
                sequence: 0,
                block_size_bytes: BLOCK_SIZE,
                drive_compression: false,
            },
            &mut bot,
        )
        .expect("BOT bootstrap");
        let mut files = vec![bot, vec![0xA5; BLOCK_SIZE as usize]];
        for ordinal in 1..=3 {
            let plan = plan_tape_index_replica(edition.clone(), ordinal).expect("replica plan");
            let mut file = Vec::new();
            let mut source = rows.clone();
            write_tape_index_replica(
                &plan,
                TapeIndexReplicaObservation {
                    tape_file_number: plan.component.planned_tape_file_number,
                    start_lba: plan.component.planned_start_lba,
                    record_count: plan.component.record_count,
                },
                &mut source,
                |block| {
                    file.extend_from_slice(block);
                    Ok(())
                },
            )
            .expect("replica bytes");
            files.push(file);
            if ordinal != 3 {
                let gap_ordinal = ordinal;
                let plan = plan_index_separation(IndexSeparationDescriptor {
                    tape_uuid: TAPE_UUID,
                    edition_id: edition.descriptor.edition_id,
                    gap_ordinal,
                    block_size: BLOCK_SIZE,
                    nominal_extent_bytes: 3 * u64::from(BLOCK_SIZE),
                    total_records: 3,
                    compression_enabled: false,
                    terminal_layout: layout,
                })
                .expect("gap plan");
                let mut gap = Vec::new();
                write_index_separation(
                    &plan,
                    IndexSeparationObservation {
                        tape_file_number: plan.component.planned_tape_file_number,
                        start_lba: plan.component.planned_start_lba,
                        record_count: plan.component.record_count,
                    },
                    |block| {
                        gap.extend_from_slice(block);
                        Ok(())
                    },
                )
                .expect("gap bytes");
                files.push(gap);
            }
        }
        ImageDirectoryRawSource::from_tape_files(files, BLOCK_SIZE).expect("image source")
    }

    #[test]
    fn hermetic_damage_matrix_and_full_verify_pass() {
        for plan in [
            TerminalIndexDamagePlan::None,
            TerminalIndexDamagePlan::A,
            TerminalIndexDamagePlan::B,
            TerminalIndexDamagePlan::C,
            TerminalIndexDamagePlan::Ab,
            TerminalIndexDamagePlan::Ac,
            TerminalIndexDamagePlan::Bc,
            TerminalIndexDamagePlan::Abc,
            TerminalIndexDamagePlan::Disagreement,
            TerminalIndexDamagePlan::GapAbHeader,
            TerminalIndexDamagePlan::GapAbFooter,
            TerminalIndexDamagePlan::GapBcHeader,
            TerminalIndexDamagePlan::GapBcFooter,
        ] {
            let full_verify =
                plan == TerminalIndexDamagePlan::None || plan.separation_damage().is_some();
            let report = inspect_source(
                fixture(),
                TAPE_UUID,
                BLOCK_SIZE,
                plan,
                None,
                full_verify,
                "hermetic_image",
            )
            .unwrap_or_else(|error| panic!("{plan:?}: {error}"));
            assert!(
                report.success,
                "{plan:?}: {:?}",
                report.expectation_failures
            );
            assert_eq!(report.fast_inventory.prefix_record_reads, 0);
            if plan == TerminalIndexDamagePlan::None {
                assert!(report.full_verify.complete);
                assert_eq!(report.full_verify.outcome, "verified_complete");
                assert_eq!(report.full_verify.terminal_components_validated, 5);
            }
            if let Some((ordinal, _)) = plan.separation_damage() {
                assert!(!report.fast_inventory.degraded);
                assert!(!report.full_verify.complete);
                assert_eq!(report.full_verify.outcome, "verified_degraded");
                assert_eq!(report.full_verify.terminal_components_validated, 4);
                assert_eq!(report.full_verify.separations.len(), 2);
                assert_eq!(
                    report.full_verify.separations[usize::from(ordinal - 1)].state,
                    "invalid"
                );
            }
            if plan == TerminalIndexDamagePlan::Abc {
                assert!(report.bot_recovery.performed);
                assert_eq!(report.bot_recovery.visited_object_count, 1);
                assert_eq!(report.bot_recovery.unknown_object_count, 1);
            }
            if plan == TerminalIndexDamagePlan::Disagreement {
                assert_eq!(report.fast_inventory.outcome, "refused");
                assert_eq!(report.fast_inventory.selected_replica_ordinal, None);
                assert_eq!(
                    report.fast_inventory.refusal_error,
                    Some("TerminalIndexReplicaConflict")
                );
                assert_eq!(report.fast_inventory.conflicting_edition_count, 2);
            }
        }
    }

    #[test]
    fn hermetic_reconciliation_drill_exercises_production_no_motion_decision() {
        for requested in [
            TerminalReconcileDrill::TornWorm,
            TerminalReconcileDrill::UnprovedStart,
        ] {
            let report = inspect_source(
                fixture(),
                TAPE_UUID,
                BLOCK_SIZE,
                TerminalIndexDamagePlan::None,
                Some(requested),
                false,
                "hermetic_image",
            )
            .unwrap_or_else(|error| panic!("{requested:?}: {error}"));
            assert!(report.success, "{:?}", report.expectation_failures);
            assert_eq!(
                report.injection_mechanism,
                "read_side_production_terminal_reconciler"
            );
            let evidence = report
                .terminal_reconciliation
                .expect("requested reconciliation report");
            assert_eq!(
                serde_json::to_value(evidence).expect("serialize reconciliation evidence"),
                serde_json::json!({
                    "requested_evidence": requested.requested_evidence(),
                    "component": "replica_a",
                    "component_start_lba": report.terminal_layout.components[0].start_lba.to_string(),
                    "injection_kind": requested.injection_kind(),
                    "outcome": "recovery_required",
                    "component_motion_attempted": false,
                    "terminal_component_admission": "refused",
                    "progress_advanced": false,
                })
            );
            assert_eq!(
                report.injected_unreadable_lbas,
                if requested == TerminalReconcileDrill::TornWorm {
                    vec![report.terminal_layout.components[0].start_lba]
                } else {
                    Vec::new()
                }
            );
            assert!(report.expectations_met);
            assert!(report.expectation_failures.is_empty());
        }
    }

    #[test]
    fn documented_cli_parses() {
        let cli = <crate::DebugCli as clap::Parser>::try_parse_from([
            "rem-debug",
            "tape",
            "terminal-index-drill",
            "--device",
            "/dev/sg7",
            "--tape-uuid",
            "71717171-7171-7171-7171-717171717171",
            "--block-size",
            "262144",
            "--damage-plan",
            "none",
            "--reconcile-outcome",
            "torn-worm",
            "--full-verify",
            "--report",
            "/tmp/tix.json",
        ])
        .expect("documented terminal-index drill parses");
        let crate::Command::Tape {
            command: crate::TapeCommand::TerminalIndexDrill(args),
        } = cli.command
        else {
            panic!("expected terminal-index drill")
        };
        assert_eq!(args.device, Path::new("/dev/sg7"));
        assert_eq!(args.block_size, BLOCK_SIZE);
        assert_eq!(args.damage_plan, TerminalIndexDamagePlan::None);
        assert_eq!(
            args.reconcile_outcome,
            Some(TerminalReconcileDrill::TornWorm)
        );
        assert!(args.full_verify);
    }

    #[test]
    fn rejects_unsupported_block_size() {
        assert!(parse_block_size("4096").is_err());
    }

    #[test]
    fn stable_report_serializes_every_u64_as_canonical_decimal_text() {
        let max = u64::MAX;
        let report = TerminalIndexDrillReport {
            schema: REPORT_SCHEMA,
            report_version: 1,
            execution: "hermetic_image",
            tape_uuid: Uuid::from_bytes(TAPE_UUID).to_string(),
            block_size_bytes: BLOCK_SIZE,
            damage_plan: TerminalIndexDamagePlan::None,
            terminal_reconciliation: None,
            injection_mechanism: "none",
            injected_unreadable_lbas: vec![0, max],
            injected_replacement_lbas: vec![max],
            terminal_layout: TerminalLayoutReport {
                partition: 0,
                expected_eod_lba: max,
                components: vec![TerminalComponentReport {
                    kind: "tape_index_replica",
                    ordinal: 1,
                    tape_file_number: max,
                    start_lba: max,
                    record_count: max,
                    footer_lba: max,
                }],
            },
            fast_inventory: FastInventoryReport {
                outcome: "complete",
                selected_replica_ordinal: Some(3),
                degraded: false,
                refusal_error: None,
                conflicting_edition_count: max,
                structural_entry_count: max,
                object_row_count: max,
                replicas: Vec::new(),
                eod_calls: max,
                backward_filemark_calls: max,
                record_reads: max,
                prefix_record_reads: max,
            },
            bot_recovery: BotRecoveryReport {
                required: false,
                performed: false,
                structural_entry_count: max,
                complete_object_count: max,
                recovered_object_count: max,
                unknown_object_count: max,
                incomplete_object_count: max,
                visited_object_count: max,
            },
            full_verify: FullVerifyReport {
                requested: true,
                outcome: "verified_complete",
                complete: true,
                measured_tape_file_count: max,
                canonical_prefix_tape_file_count: max,
                terminal_components_validated: max,
                truncation_present: false,
                separations: Vec::new(),
            },
            capabilities_exercised: Vec::new(),
            expectations_met: true,
            expectation_failures: Vec::new(),
            success: true,
        };

        let encoded = serde_json::to_value(report).expect("stable report serializes");
        let decimal_max = serde_json::Value::String(max.to_string());
        assert_eq!(encoded["injected_unreadable_lbas"][0], "0");
        assert_eq!(encoded["injected_unreadable_lbas"][1], decimal_max);
        assert_eq!(encoded["injected_replacement_lbas"][0], decimal_max);
        for pointer in [
            "/fast_inventory/structural_entry_count",
            "/fast_inventory/object_row_count",
            "/fast_inventory/eod_calls",
            "/fast_inventory/backward_filemark_calls",
            "/fast_inventory/record_reads",
            "/fast_inventory/prefix_record_reads",
            "/fast_inventory/conflicting_edition_count",
            "/bot_recovery/structural_entry_count",
            "/bot_recovery/complete_object_count",
            "/bot_recovery/recovered_object_count",
            "/bot_recovery/unknown_object_count",
            "/bot_recovery/incomplete_object_count",
            "/bot_recovery/visited_object_count",
            "/full_verify/measured_tape_file_count",
            "/full_verify/canonical_prefix_tape_file_count",
            "/full_verify/terminal_components_validated",
            "/terminal_layout/expected_eod_lba",
            "/terminal_layout/components/0/tape_file_number",
            "/terminal_layout/components/0/start_lba",
            "/terminal_layout/components/0/record_count",
            "/terminal_layout/components/0/footer_lba",
        ] {
            assert_eq!(encoded.pointer(pointer), Some(&decimal_max), "{pointer}");
        }
        assert_eq!(encoded["report_version"], 1);
        assert_eq!(encoded["block_size_bytes"], BLOCK_SIZE);
        assert_eq!(encoded["fast_inventory"]["selected_replica_ordinal"], 3);
    }

    #[test]
    fn object_row_type_remains_constructible_for_nonempty_fixtures() {
        let row = TapeIndexReplicaObjectRow {
            tape_file_number: 1,
            stored_block_count: 1,
            object_id: b"object".to_vec(),
            representation: ObjectRecoveryRepresentation::Plaintext {
                manifest_first_chunk_lba: 0,
                manifest_size_bytes: 1,
                manifest_chunk_count: 1,
                manifest_sha256: [0; 32],
            },
        };
        assert_eq!(row.stored_block_count, 1);
    }
}
