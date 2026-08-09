//! Explicitly gated terminal-finalization crash cuts for system scenario TIX.
//!
//! This is not a remotely reachable API. The daemon process must be launched
//! with the exact opt-in environment value and a root-confined, exact-tape
//! plan. A matching cut fsyncs one evidence record and aborts the process.
//! Existing evidence makes the same plan one-shot across the restart.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use remanence_parity::{
    PhysicalPositionHint, RawTapeSink, RawWriteOutcome, TerminalPrefixPlan,
    TerminalTailComponentKind, TerminalTailComponentPlan,
};
use serde_json::{json, Value};
use uuid::Uuid;

pub(crate) const ENABLE_ENV: &str = "REM_TIX_TERMINAL_FAULT_ENABLE";
pub(crate) const PLAN_ENV: &str = "REM_TIX_TERMINAL_FAULT_PLAN";
const ENABLE_VALUE: &str = "terminal-index-stage7-v1-abort";
const PLAN_SCHEMA: &str = "rem.tix.terminal-fault-plan.v1";
const EVIDENCE_SCHEMA: &str = "rem.tix.terminal-fault-evidence.v1";
const TIX_FAULT_ROOT: &str = "/tmp/system-harness/scenario-tix/terminal-faults";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalFaultCut {
    BeforeTerminalPrefix,
    BeforeFinalParityMap,
    AfterFinalParityMap,
    BeforeFooter,
    AfterFooter,
    BeforeFilemark,
    AfterFilemark,
    BeforeBarrier,
    AfterBarrier,
    BeforeParityJournalFsync,
    AfterParityJournalFsync,
    BeforeCheckpointJournalFsync,
    AfterCheckpointJournalFsync,
    BeforeSqliteProjection,
    AfterSqliteProjection,
    AfterTerminalPrefix,
    BeforeFinalCheckpointFsync,
    AfterFinalCheckpointFsync,
    BeforeFinalSqliteProjection,
    AfterFinalSqliteProjection,
    BeforeAssignmentReread,
}

impl TerminalFaultCut {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::BeforeTerminalPrefix => "before_terminal_prefix",
            Self::BeforeFinalParityMap => "before_final_parity_map",
            Self::AfterFinalParityMap => "after_final_parity_map",
            Self::BeforeFooter => "before_footer",
            Self::AfterFooter => "after_footer",
            Self::BeforeFilemark => "before_filemark",
            Self::AfterFilemark => "after_filemark",
            Self::BeforeBarrier => "before_barrier",
            Self::AfterBarrier => "after_barrier",
            Self::BeforeParityJournalFsync => "before_parity_journal_fsync",
            Self::AfterParityJournalFsync => "after_parity_journal_fsync",
            Self::BeforeCheckpointJournalFsync => "before_checkpoint_journal_fsync",
            Self::AfterCheckpointJournalFsync => "after_checkpoint_journal_fsync",
            Self::BeforeSqliteProjection => "before_sqlite_projection",
            Self::AfterSqliteProjection => "after_sqlite_projection",
            Self::AfterTerminalPrefix => "after_terminal_prefix",
            Self::BeforeFinalCheckpointFsync => "before_final_checkpoint_fsync",
            Self::AfterFinalCheckpointFsync => "after_final_checkpoint_fsync",
            Self::BeforeFinalSqliteProjection => "before_final_sqlite_projection",
            Self::AfterFinalSqliteProjection => "after_final_sqlite_projection",
            Self::BeforeAssignmentReread => "before_assignment_reread",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "before_terminal_prefix" => Self::BeforeTerminalPrefix,
            "before_final_parity_map" => Self::BeforeFinalParityMap,
            "after_final_parity_map" => Self::AfterFinalParityMap,
            "before_footer" => Self::BeforeFooter,
            "after_footer" => Self::AfterFooter,
            "before_filemark" => Self::BeforeFilemark,
            "after_filemark" => Self::AfterFilemark,
            "before_barrier" => Self::BeforeBarrier,
            "after_barrier" => Self::AfterBarrier,
            "before_parity_journal_fsync" => Self::BeforeParityJournalFsync,
            "after_parity_journal_fsync" => Self::AfterParityJournalFsync,
            "before_checkpoint_journal_fsync" => Self::BeforeCheckpointJournalFsync,
            "after_checkpoint_journal_fsync" => Self::AfterCheckpointJournalFsync,
            "before_sqlite_projection" => Self::BeforeSqliteProjection,
            "after_sqlite_projection" => Self::AfterSqliteProjection,
            "after_terminal_prefix" => Self::AfterTerminalPrefix,
            "before_final_checkpoint_fsync" => Self::BeforeFinalCheckpointFsync,
            "after_final_checkpoint_fsync" => Self::AfterFinalCheckpointFsync,
            "before_final_sqlite_projection" => Self::BeforeFinalSqliteProjection,
            "after_final_sqlite_projection" => Self::AfterFinalSqliteProjection,
            "before_assignment_reread" => Self::BeforeAssignmentReread,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalFaultPlan {
    tape_uuid: [u8; 16],
    component: String,
    cut: TerminalFaultCut,
    nonce: Uuid,
    evidence_path: PathBuf,
}

impl TerminalFaultPlan {
    /// Load an exact TIX plan. Disabled operation is the default.
    pub(crate) fn from_env_for_tape(tape_uuid: [u8; 16]) -> Result<Option<Self>, String> {
        let enabled = std::env::var_os(ENABLE_ENV);
        let plan_path = std::env::var_os(PLAN_ENV);
        if enabled.is_none() && plan_path.is_none() {
            return Ok(None);
        }
        let enabled = enabled.ok_or_else(|| {
            format!("{PLAN_ENV} is set but the explicit {ENABLE_ENV} opt-in is absent")
        })?;
        if enabled != OsStr::new(ENABLE_VALUE) {
            return Err(format!(
                "{ENABLE_ENV} must equal the exact TIX-only value {ENABLE_VALUE:?}"
            ));
        }
        let plan_path = PathBuf::from(plan_path.ok_or_else(|| {
            format!("{ENABLE_ENV} is set but the exact {PLAN_ENV} path is absent")
        })?);
        require_confined_path(&plan_path, "fault plan")?;
        let bytes = fs::read(&plan_path).map_err(|error| {
            format!(
                "read TIX terminal fault plan {}: {error}",
                plan_path.display()
            )
        })?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "decode TIX terminal fault plan {}: {error}",
                plan_path.display()
            )
        })?;
        let plan = Self::parse(&value)?;
        if plan.tape_uuid != tape_uuid {
            return Ok(None);
        }
        Ok(Some(plan))
    }

    fn parse(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| "TIX terminal fault plan must be a JSON object".to_string())?;
        let required = [
            "schema",
            "tape_uuid",
            "component",
            "cut",
            "nonce",
            "evidence_path",
        ];
        if object.len() != required.len() || required.iter().any(|key| !object.contains_key(*key)) {
            return Err(format!(
                "TIX terminal fault plan must contain exactly {}",
                required.join(", ")
            ));
        }
        require_text(object, "schema", PLAN_SCHEMA)?;
        let tape_uuid_text = text_field(object, "tape_uuid")?;
        let tape_uuid = Uuid::parse_str(tape_uuid_text)
            .map_err(|error| format!("invalid terminal fault tape_uuid: {error}"))?;
        if tape_uuid.to_string() != tape_uuid_text {
            return Err(
                "terminal fault tape_uuid must be canonical lowercase UUID text".to_string(),
            );
        }
        let component = text_field(object, "component")?.to_string();
        if !matches!(
            component.as_str(),
            "parity_closeout"
                | "replica_a"
                | "separation_ab"
                | "replica_b"
                | "separation_bc"
                | "replica_c"
                | "final_projection"
                | "assignment_race"
        ) {
            return Err(format!(
                "unsupported terminal fault component {component:?}"
            ));
        }
        let cut_text = text_field(object, "cut")?;
        let cut = TerminalFaultCut::parse(cut_text)
            .ok_or_else(|| format!("unsupported terminal fault cut {cut_text:?}"))?;
        validate_component_cut(&component, cut)?;
        let nonce_text = text_field(object, "nonce")?;
        let nonce = Uuid::parse_str(nonce_text)
            .map_err(|error| format!("invalid terminal fault nonce: {error}"))?;
        if nonce.is_nil() || nonce.to_string() != nonce_text {
            return Err("terminal fault nonce must be a non-nil canonical UUID".to_string());
        }
        let evidence_path = PathBuf::from(text_field(object, "evidence_path")?);
        require_confined_path(&evidence_path, "evidence")?;
        Ok(Self {
            tape_uuid: *tape_uuid.as_bytes(),
            component,
            cut,
            nonce,
            evidence_path,
        })
    }

    pub(crate) fn abort_if_matches(
        &self,
        component: &str,
        cut: TerminalFaultCut,
        position: Option<PhysicalPositionHint>,
        plan: Option<TerminalTailComponentPlan>,
    ) -> Result<(), String> {
        if self.record_if_matches(component, cut, position, plan)? {
            std::process::abort();
        }
        Ok(())
    }

    pub(crate) fn abort_component_if_matches(
        &self,
        component: TerminalTailComponentPlan,
        cut: TerminalFaultCut,
        position: Option<PhysicalPositionHint>,
    ) -> Result<(), String> {
        self.abort_if_matches(component_name(component), cut, position, Some(component))
    }

    /// Abort at one exact terminal-prefix boundary after publishing typed
    /// evidence about whether a sidecar or ParityMap still requires motion.
    pub(crate) fn abort_prefix_if_matches(
        &self,
        cut: TerminalFaultCut,
        position: Option<PhysicalPositionHint>,
        plan: &TerminalPrefixPlan,
    ) -> Result<(), String> {
        if self.record_prefix_if_matches(cut, position, plan)? {
            std::process::abort();
        }
        Ok(())
    }

    /// Exercise the exact conditional-generation race required by system TIX.
    ///
    /// The public FinalizeTape path has already captured `expected_generation`
    /// in its request. This one-shot hook clears that exact tape's assignment
    /// through the ordinary CAS catalog method immediately before the owner
    /// rereads it. The ensuing production guard must reject without publishing
    /// Finalizing or acquiring a drive.
    pub(crate) fn clear_assignment_before_reread(
        &self,
        index: &mut remanence_state::CatalogIndex,
        tape_uuid: [u8; 16],
        expected_generation: u64,
        expected_pool_id: Option<&str>,
    ) -> Result<(), String> {
        if self.component != "assignment_race"
            || self.cut != TerminalFaultCut::BeforeAssignmentReread
        {
            return Ok(());
        }
        if self.evidence_path.exists() {
            self.validate_existing_evidence()?;
            return Ok(());
        }
        if self.tape_uuid != tape_uuid {
            return Err("TIX assignment-race plan reached the wrong exact tape".to_string());
        }
        let expected_pool_id = expected_pool_id.ok_or_else(|| {
            "TIX assignment-race hook requires the pooled FinalizeTape guard".to_string()
        })?;
        let before = index
            .get_tape_assignment_snapshot(&tape_uuid)
            .map_err(|error| format!("read TIX assignment-race snapshot: {error}"))?
            .ok_or_else(|| "TIX assignment-race tape disappeared".to_string())?;
        if before.assignment_generation != expected_generation
            || before.pool_id.as_deref() != Some(expected_pool_id)
        {
            return Err(format!(
                "TIX assignment-race precondition changed: expected generation {expected_generation} pool {expected_pool_id:?}, found generation {} pool {:?}",
                before.assignment_generation, before.pool_id
            ));
        }
        let after = index
            .compare_and_set_tape_pool_assignment(tape_uuid, expected_generation, None)
            .map_err(|error| format!("commit TIX assignment-race CAS: {error}"))?;
        let evidence = json!({
            "schema": EVIDENCE_SCHEMA,
            "tape_uuid": Uuid::from_bytes(self.tape_uuid).to_string(),
            "component": self.component,
            "cut": self.cut.name(),
            "nonce": self.nonce.to_string(),
            "process_id": std::process::id(),
            "assignment_race": {
                "before_pool_id": expected_pool_id,
                "before_generation": expected_generation.to_string(),
                "after_pool_id": after.pool_id,
                "after_generation": after.assignment_generation.to_string(),
            },
        });
        write_durable_new_json(&self.evidence_path, &evidence)
    }

    fn record_if_matches(
        &self,
        component: &str,
        cut: TerminalFaultCut,
        position: Option<PhysicalPositionHint>,
        plan: Option<TerminalTailComponentPlan>,
    ) -> Result<bool, String> {
        if self.component != component || self.cut != cut {
            return Ok(false);
        }
        if self.evidence_path.exists() {
            self.validate_existing_evidence()?;
            return Ok(false);
        }
        let evidence = json!({
            "schema": EVIDENCE_SCHEMA,
            "tape_uuid": Uuid::from_bytes(self.tape_uuid).to_string(),
            "component": component,
            "cut": cut.name(),
            "nonce": self.nonce.to_string(),
            "process_id": std::process::id(),
            "position": position.map(|position| json!({
                "partition": position.partition,
                "lba": position.lba.to_string(),
            })),
            "component_plan": plan.map(|plan| json!({
                "kind": match plan.kind {
                    TerminalTailComponentKind::TapeIndexReplica => "tape_index_replica",
                    TerminalTailComponentKind::IndexSeparationExtent => "index_separation_extent",
                },
                "ordinal": plan.ordinal,
                "planned_tape_file_number": plan.planned_tape_file_number.to_string(),
                "planned_start_lba": plan.planned_start_lba.to_string(),
                "record_count": plan.record_count.to_string(),
            })),
        });
        write_durable_new_json(&self.evidence_path, &evidence)?;
        Ok(true)
    }

    fn record_prefix_if_matches(
        &self,
        cut: TerminalFaultCut,
        position: Option<PhysicalPositionHint>,
        plan: &TerminalPrefixPlan,
    ) -> Result<bool, String> {
        if self.component != "parity_closeout" || self.cut != cut {
            return Ok(false);
        }
        if self.evidence_path.exists() {
            self.validate_existing_evidence()?;
            return Ok(false);
        }
        let final_sidecar = if plan.sidecar_directory_entries.is_empty() {
            "not_required"
        } else {
            // The daemon terminal-finalization path closes an already durable
            // ordinary checkpoint. Its partial sidecar is therefore prefix
            // authority, not a terminal-only write waiting behind Finalizing.
            "already_committed_prefix"
        };
        let final_parity_map = if plan.parity_map_tape_file_number.is_some() {
            "required"
        } else {
            "not_required"
        };
        let evidence = json!({
            "schema": EVIDENCE_SCHEMA,
            "tape_uuid": Uuid::from_bytes(self.tape_uuid).to_string(),
            "component": self.component,
            "cut": cut.name(),
            "nonce": self.nonce.to_string(),
            "process_id": std::process::id(),
            "position": position.map(|position| json!({
                "partition": position.partition,
                "lba": position.lba.to_string(),
            })),
            "terminal_prefix": {
                "start_tape_file_number": plan.start_tape_file_number.to_string(),
                "tail_start_tape_file_number": plan.tail_start_tape_file_number.to_string(),
                "start_lba": plan.start_lba.to_string(),
                "tail_start_lba": plan.tail_start_lba.to_string(),
                "parity_map_tape_file_number": plan
                    .parity_map_tape_file_number
                    .map(|value| value.to_string()),
                "final_sidecar": final_sidecar,
                "final_parity_map": final_parity_map,
            },
        });
        write_durable_new_json(&self.evidence_path, &evidence)?;
        Ok(true)
    }

    fn validate_existing_evidence(&self) -> Result<(), String> {
        let bytes = fs::read(&self.evidence_path).map_err(|error| {
            format!(
                "read existing terminal fault evidence {}: {error}",
                self.evidence_path.display()
            )
        })?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            format!(
                "decode existing terminal fault evidence {}: {error}",
                self.evidence_path.display()
            )
        })?;
        for (field, expected) in [
            ("schema", EVIDENCE_SCHEMA.to_string()),
            ("tape_uuid", Uuid::from_bytes(self.tape_uuid).to_string()),
            ("component", self.component.clone()),
            ("cut", self.cut.name().to_string()),
            ("nonce", self.nonce.to_string()),
        ] {
            if value.get(field).and_then(Value::as_str) != Some(expected.as_str()) {
                return Err(format!(
                    "existing terminal fault evidence {} has mismatched {field}",
                    self.evidence_path.display()
                ));
            }
        }
        Ok(())
    }
}

fn component_name(component: TerminalTailComponentPlan) -> &'static str {
    match (component.kind, component.ordinal) {
        (TerminalTailComponentKind::TapeIndexReplica, 1) => "replica_a",
        (TerminalTailComponentKind::IndexSeparationExtent, 1) => "separation_ab",
        (TerminalTailComponentKind::TapeIndexReplica, 2) => "replica_b",
        (TerminalTailComponentKind::IndexSeparationExtent, 2) => "separation_bc",
        (TerminalTailComponentKind::TapeIndexReplica, 3) => "replica_c",
        _ => "invalid_terminal_component",
    }
}

fn validate_component_cut(component: &str, cut: TerminalFaultCut) -> Result<(), String> {
    let valid = match component {
        "parity_closeout" => matches!(
            cut,
            TerminalFaultCut::BeforeTerminalPrefix
                | TerminalFaultCut::BeforeFinalParityMap
                | TerminalFaultCut::AfterFinalParityMap
                | TerminalFaultCut::AfterTerminalPrefix
        ),
        "final_projection" => matches!(
            cut,
            TerminalFaultCut::BeforeFinalCheckpointFsync
                | TerminalFaultCut::AfterFinalCheckpointFsync
                | TerminalFaultCut::BeforeFinalSqliteProjection
                | TerminalFaultCut::AfterFinalSqliteProjection
        ),
        "assignment_race" => cut == TerminalFaultCut::BeforeAssignmentReread,
        _ => !matches!(
            cut,
            TerminalFaultCut::BeforeTerminalPrefix
                | TerminalFaultCut::BeforeFinalParityMap
                | TerminalFaultCut::AfterFinalParityMap
                | TerminalFaultCut::AfterTerminalPrefix
                | TerminalFaultCut::BeforeFinalCheckpointFsync
                | TerminalFaultCut::AfterFinalCheckpointFsync
                | TerminalFaultCut::BeforeFinalSqliteProjection
                | TerminalFaultCut::AfterFinalSqliteProjection
                | TerminalFaultCut::BeforeAssignmentReread
        ),
    };
    if valid {
        Ok(())
    } else {
        Err(format!(
            "terminal fault cut {:?} is invalid for component {component:?}",
            cut.name()
        ))
    }
}

/// Fault-aware adapter for the final ParityMap portion of the terminal prefix.
/// The daemon path's sidecar, when present, is already ordinary checkpoint
/// authority; this adapter therefore never claims a sidecar emission cut.
pub(crate) struct TerminalPrefixFaultSink<'a, S: RawTapeSink + ?Sized> {
    inner: &'a mut S,
    fault: Option<&'a TerminalFaultPlan>,
    plan: &'a TerminalPrefixPlan,
    parity_map_record_count: Option<u64>,
    records_written: u64,
}

impl<'a, S: RawTapeSink + ?Sized> TerminalPrefixFaultSink<'a, S> {
    pub(crate) fn new(
        inner: &'a mut S,
        fault: Option<&'a TerminalFaultPlan>,
        plan: &'a TerminalPrefixPlan,
    ) -> Self {
        let parity_map_record_count = plan.parity_map_tape_file_number.and_then(|file_number| {
            plan.committed_bundle
                .entries
                .iter()
                .find(|entry| {
                    entry.tape_file_number == file_number
                        && entry.kind == remanence_parity::TapeFileKind::ParityMap
                })
                .map(|entry| entry.block_count)
        });
        Self {
            inner,
            fault,
            plan,
            parity_map_record_count,
            records_written: 0,
        }
    }

    fn cut(
        &self,
        cut: TerminalFaultCut,
        position: Option<PhysicalPositionHint>,
    ) -> Result<(), remanence_parity::ParityError> {
        let Some(fault) = self.fault else {
            return Ok(());
        };
        fault
            .abort_prefix_if_matches(cut, position, self.plan)
            .map_err(remanence_parity::ParityError::SessionOpen)
    }
}

impl<S: RawTapeSink + ?Sized> RawTapeSink for TerminalPrefixFaultSink<'_, S> {
    fn locate_for_overwrite(
        &mut self,
        hint: PhysicalPositionHint,
    ) -> Result<(), remanence_parity::ParityError> {
        self.inner.locate_for_overwrite(hint)
    }

    fn write_fixed_block(
        &mut self,
        buf: &[u8],
    ) -> Result<RawWriteOutcome, remanence_parity::ParityError> {
        if self.records_written == 0 && self.parity_map_record_count.is_some() {
            let position = self.inner.position().ok();
            self.cut(TerminalFaultCut::BeforeFinalParityMap, position)?;
        }
        let outcome = self.inner.write_fixed_block(buf)?;
        self.records_written =
            self.records_written
                .checked_add(1)
                .ok_or(remanence_parity::ParityError::Invariant(
                    "TIX terminal-prefix fault record counter overflow",
                ))?;
        Ok(outcome)
    }

    fn write_filemarks(
        &mut self,
        count: u32,
        immediate: bool,
    ) -> Result<RawWriteOutcome, remanence_parity::ParityError> {
        let outcome = self.inner.write_filemarks(count, immediate)?;
        if count == 1 && self.parity_map_record_count == Some(self.records_written) {
            self.cut(
                TerminalFaultCut::AfterFinalParityMap,
                Some(outcome.position_after()),
            )?;
        }
        Ok(outcome)
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, remanence_parity::ParityError> {
        self.inner.position()
    }
}

fn text_field<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("terminal fault plan field {field:?} must be nonempty text"))
}

fn require_text(
    object: &serde_json::Map<String, Value>,
    field: &str,
    expected: &str,
) -> Result<(), String> {
    let actual = text_field(object, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "terminal fault plan field {field:?} must equal {expected:?}, got {actual:?}"
        ))
    }
}

fn require_confined_path(path: &Path, label: &str) -> Result<(), String> {
    if !path.is_absolute()
        || !path.starts_with(TIX_FAULT_ROOT)
        || path.components().any(|part| part.as_os_str() == "..")
    {
        return Err(format!(
            "TIX terminal fault {label} path {} must be absolute and confined beneath {TIX_FAULT_ROOT}",
            path.display()
        ));
    }
    Ok(())
}

fn write_durable_new_json(path: &Path, value: &Value) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("terminal fault evidence {} has no parent", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "create terminal fault evidence directory {}: {error}",
            parent.display()
        )
    })?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| {
            format!(
                "create terminal fault evidence {}: {error}",
                temporary.display()
            )
        })?;
    serde_json::to_writer_pretty(&mut file, value)
        .map_err(|error| format!("encode terminal fault evidence: {error}"))?;
    file.write_all(b"\n")
        .map_err(|error| format!("finish terminal fault evidence: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("fsync terminal fault evidence: {error}"))?;
    drop(file);
    fs::rename(&temporary, path).map_err(|error| {
        format!(
            "publish terminal fault evidence {}: {error}",
            path.display()
        )
    })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("fsync terminal fault evidence directory: {error}"))
}

pub(crate) struct TerminalFaultSink<'a, S: RawTapeSink + ?Sized> {
    inner: &'a mut S,
    fault: Option<&'a TerminalFaultPlan>,
    component: TerminalTailComponentPlan,
    records_written: u64,
}

impl<'a, S: RawTapeSink + ?Sized> TerminalFaultSink<'a, S> {
    pub(crate) fn new(
        inner: &'a mut S,
        fault: Option<&'a TerminalFaultPlan>,
        component: TerminalTailComponentPlan,
    ) -> Self {
        Self {
            inner,
            fault,
            component,
            records_written: 0,
        }
    }

    fn cut(
        &self,
        cut: TerminalFaultCut,
        position: Option<PhysicalPositionHint>,
    ) -> Result<(), remanence_parity::ParityError> {
        let Some(fault) = self.fault else {
            return Ok(());
        };
        fault
            .abort_if_matches(
                component_name(self.component),
                cut,
                position,
                Some(self.component),
            )
            .map_err(remanence_parity::ParityError::SessionOpen)
    }
}

impl<S: RawTapeSink + ?Sized> RawTapeSink for TerminalFaultSink<'_, S> {
    fn locate_for_overwrite(
        &mut self,
        hint: PhysicalPositionHint,
    ) -> Result<(), remanence_parity::ParityError> {
        self.inner.locate_for_overwrite(hint)
    }

    fn write_fixed_block(
        &mut self,
        buf: &[u8],
    ) -> Result<RawWriteOutcome, remanence_parity::ParityError> {
        let is_footer = self.records_written.checked_add(1) == Some(self.component.record_count);
        if is_footer {
            let position = self.inner.position().ok();
            self.cut(TerminalFaultCut::BeforeFooter, position)?;
        }
        let outcome = self.inner.write_fixed_block(buf)?;
        self.records_written =
            self.records_written
                .checked_add(1)
                .ok_or(remanence_parity::ParityError::Invariant(
                    "TIX terminal fault record counter overflow",
                ))?;
        if is_footer {
            self.cut(
                TerminalFaultCut::AfterFooter,
                Some(outcome.position_after()),
            )?;
        }
        Ok(outcome)
    }

    fn write_filemarks(
        &mut self,
        count: u32,
        immediate: bool,
    ) -> Result<RawWriteOutcome, remanence_parity::ParityError> {
        let (before, after) = if count == 0 {
            (
                TerminalFaultCut::BeforeBarrier,
                TerminalFaultCut::AfterBarrier,
            )
        } else if count == 1 {
            (
                TerminalFaultCut::BeforeFilemark,
                TerminalFaultCut::AfterFilemark,
            )
        } else {
            return self.inner.write_filemarks(count, immediate);
        };
        let position = self.inner.position().ok();
        self.cut(before, position)?;
        let outcome = self.inner.write_filemarks(count, immediate)?;
        self.cut(after, Some(outcome.position_after()))?;
        Ok(outcome)
    }

    fn position(&mut self) -> Result<PhysicalPositionHint, remanence_parity::ParityError> {
        self.inner.position()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use remanence_parity::{
        CommittedBundle, CommittedBundleKind, ParityConfig, SidecarEpochDirectoryEntry,
        TapeFileEntry, TapeFileKind, TerminalTailComponentKind,
    };
    use remanence_state::{CatalogIndex, ProvisionTapeInput, TapePoolProjectionInput};

    fn plan_value(evidence: &Path) -> Value {
        json!({
            "schema": PLAN_SCHEMA,
            "tape_uuid": "71717171-7171-7171-7171-717171717171",
            "component": "replica_a",
            "cut": "after_barrier",
            "nonce": "81818181-8181-8181-8181-818181818181",
            "evidence_path": evidence,
        })
    }

    fn terminal_prefix_plan() -> TerminalPrefixPlan {
        TerminalPrefixPlan {
            start_tape_file_number: 9,
            tail_start_tape_file_number: 10,
            start_lba: 91,
            tail_start_lba: 95,
            parity_map_tape_file_number: Some(9),
            sidecar_directory_entries: vec![SidecarEpochDirectoryEntry {
                tape_file_number: 8,
                epoch_id: 1,
                protected_ordinal_start: 0,
                protected_ordinal_end_exclusive: 1,
                sidecar_total_block_count: 3,
                sidecar_header_block_count: 1,
                parity_shard_block_count: 1,
                canonical_metadata_hash: [0x91; 32],
                flags: 0,
            }],
            committed_bundle: CommittedBundle {
                kind: CommittedBundleKind::TerminalPrefix,
                entries: vec![TapeFileEntry {
                    tape_file_number: 9,
                    kind: TapeFileKind::ParityMap,
                    block_count: 3,
                    physical_start_hint: Some(91),
                    object_id: None,
                    first_parity_data_ordinal: None,
                    epoch_id: None,
                    protected_ordinal_start: None,
                    protected_ordinal_end_exclusive: None,
                    canonical_metadata_hash: Some([0x92; 32]),
                    object_recovery_row: None,
                }],
                highest_protected_ordinal: 1,
                total_committed_ordinals: 1,
            },
        }
    }

    #[derive(Default)]
    struct CountingRawSink {
        position: u64,
        blocks: u64,
        filemark_calls: u64,
    }

    impl RawTapeSink for CountingRawSink {
        fn write_fixed_block(
            &mut self,
            buf: &[u8],
        ) -> Result<RawWriteOutcome, remanence_parity::ParityError> {
            self.blocks += 1;
            self.position += 1;
            Ok(RawWriteOutcome::WroteBlock {
                bytes_written: u32::try_from(buf.len()).expect("test block size fits u32"),
                position_after: PhysicalPositionHint::new(self.position),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn write_filemarks(
            &mut self,
            count: u32,
            _immediate: bool,
        ) -> Result<RawWriteOutcome, remanence_parity::ParityError> {
            self.filemark_calls += 1;
            self.position += u64::from(count);
            Ok(RawWriteOutcome::WroteFilemark {
                position_after: PhysicalPositionHint::new(self.position),
                early_warning: false,
                end_of_medium: false,
            })
        }

        fn position(&mut self) -> Result<PhysicalPositionHint, remanence_parity::ParityError> {
            Ok(PhysicalPositionHint::new(self.position))
        }
    }

    #[test]
    fn plan_is_exact_and_rejects_unconfined_or_cross_component_cuts() {
        let root = Path::new(TIX_FAULT_ROOT);
        let valid = root.join("contract/evidence.json");
        TerminalFaultPlan::parse(&plan_value(&valid)).expect("exact plan");

        let mut outside = plan_value(Path::new("/tmp/outside.json"));
        assert!(TerminalFaultPlan::parse(&outside).is_err());
        outside["evidence_path"] = Value::String(valid.display().to_string());
        outside["component"] = Value::String("parity_closeout".to_string());
        assert!(TerminalFaultPlan::parse(&outside).is_err());
        for cut in [
            "before_terminal_prefix",
            "before_final_parity_map",
            "after_final_parity_map",
            "after_terminal_prefix",
        ] {
            outside["cut"] = Value::String(cut.to_string());
            TerminalFaultPlan::parse(&outside).expect("exact parity-closeout cut");
        }
        outside["cut"] = Value::String("after_barrier".to_string());
        assert!(TerminalFaultPlan::parse(&outside).is_err());
        outside["component"] = Value::String("replica_a".to_string());
        outside["extra"] = Value::Bool(true);
        assert!(TerminalFaultPlan::parse(&outside).is_err());

        let mut final_before = plan_value(&valid);
        final_before["component"] = Value::String("final_projection".to_string());
        final_before["cut"] = Value::String("before_final_checkpoint_fsync".to_string());
        TerminalFaultPlan::parse(&final_before).expect("final checkpoint before cut");
        final_before["cut"] = Value::String("before_final_sqlite_projection".to_string());
        TerminalFaultPlan::parse(&final_before).expect("final SQLite before cut");

        let mut assignment = plan_value(&valid);
        assignment["component"] = Value::String("assignment_race".to_string());
        assignment["cut"] = Value::String("before_assignment_reread".to_string());
        TerminalFaultPlan::parse(&assignment).expect("assignment reread race cut");
        assignment["cut"] = Value::String("after_barrier".to_string());
        assert!(TerminalFaultPlan::parse(&assignment).is_err());
    }

    #[test]
    fn matching_cut_writes_lossless_durable_one_shot_evidence() {
        let root = Path::new(TIX_FAULT_ROOT).join(format!("contract-{}", Uuid::new_v4()));
        let evidence = root.join("evidence.json");
        let plan = TerminalFaultPlan::parse(&plan_value(&evidence)).expect("exact plan");
        let component = TerminalTailComponentPlan {
            kind: TerminalTailComponentKind::TapeIndexReplica,
            ordinal: 1,
            planned_tape_file_number: u64::MAX,
            planned_start_lba: u64::MAX,
            record_count: u64::MAX,
        };
        assert!(plan
            .record_if_matches(
                "replica_a",
                TerminalFaultCut::AfterBarrier,
                Some(PhysicalPositionHint {
                    partition: 0,
                    lba: u64::MAX,
                }),
                Some(component),
            )
            .expect("record first cut"));
        assert!(!plan
            .record_if_matches("replica_a", TerminalFaultCut::AfterBarrier, None, None,)
            .expect("same plan is one shot"));
        let value: Value = serde_json::from_slice(&fs::read(&evidence).expect("evidence bytes"))
            .expect("evidence JSON");
        assert_eq!(value["position"]["lba"], u64::MAX.to_string());
        assert_eq!(
            value["component_plan"]["planned_tape_file_number"],
            u64::MAX.to_string()
        );
        assert_eq!(
            value["component_plan"]["record_count"],
            u64::MAX.to_string()
        );
        fs::remove_dir_all(root).expect("remove contract evidence");
    }

    #[test]
    fn prefix_cuts_report_committed_sidecar_and_pending_parity_map_exactly() {
        for (index, cut) in [
            TerminalFaultCut::BeforeTerminalPrefix,
            TerminalFaultCut::BeforeFinalParityMap,
            TerminalFaultCut::AfterFinalParityMap,
            TerminalFaultCut::AfterTerminalPrefix,
        ]
        .into_iter()
        .enumerate()
        {
            let root = Path::new(TIX_FAULT_ROOT)
                .join(format!("prefix-contract-{index}-{}", Uuid::new_v4()));
            let evidence = root.join("evidence.json");
            let mut value = plan_value(&evidence);
            value["component"] = Value::String("parity_closeout".to_string());
            value["cut"] = Value::String(cut.name().to_string());
            let plan = TerminalFaultPlan::parse(&value).expect("prefix fault plan");
            assert!(plan
                .record_prefix_if_matches(
                    cut,
                    Some(PhysicalPositionHint::new(91)),
                    &terminal_prefix_plan(),
                )
                .expect("record prefix cut"));
            assert!(!plan
                .record_prefix_if_matches(cut, None, &terminal_prefix_plan())
                .expect("prefix cut is one shot"));
            let recorded: Value =
                serde_json::from_slice(&fs::read(&evidence).expect("prefix evidence"))
                    .expect("prefix evidence JSON");
            assert_eq!(recorded["cut"], cut.name());
            assert_eq!(
                recorded["terminal_prefix"]["final_sidecar"],
                "already_committed_prefix"
            );
            assert_eq!(recorded["terminal_prefix"]["final_parity_map"], "required");
            assert_eq!(
                recorded["terminal_prefix"]["parity_map_tape_file_number"],
                "9"
            );
            fs::remove_dir_all(root).expect("remove prefix evidence");
        }

        let mut no_parity_map = terminal_prefix_plan();
        no_parity_map.sidecar_directory_entries.clear();
        no_parity_map.parity_map_tape_file_number = None;
        let root = Path::new(TIX_FAULT_ROOT).join(format!("prefix-none-{}", Uuid::new_v4()));
        let evidence = root.join("evidence.json");
        let mut value = plan_value(&evidence);
        value["component"] = Value::String("parity_closeout".to_string());
        value["cut"] = Value::String("before_terminal_prefix".to_string());
        let plan = TerminalFaultPlan::parse(&value).expect("empty prefix fault plan");
        assert!(plan
            .record_prefix_if_matches(
                TerminalFaultCut::BeforeTerminalPrefix,
                Some(PhysicalPositionHint::new(91)),
                &no_parity_map,
            )
            .expect("record empty prefix"));
        let recorded: Value = serde_json::from_slice(&fs::read(&evidence).expect("empty evidence"))
            .expect("empty evidence JSON");
        assert_eq!(recorded["terminal_prefix"]["final_sidecar"], "not_required");
        assert_eq!(
            recorded["terminal_prefix"]["final_parity_map"],
            "not_required"
        );
        assert!(recorded["terminal_prefix"]["parity_map_tape_file_number"].is_null());
        fs::remove_dir_all(root).expect("remove empty prefix evidence");
    }

    #[test]
    fn prefix_sink_reaches_exact_pre_block_and_post_filemark_cuts() {
        for (cut, expected_blocks, expected_filemarks) in [
            (TerminalFaultCut::BeforeFinalParityMap, 0, 0),
            (TerminalFaultCut::AfterFinalParityMap, 3, 1),
        ] {
            let root = Path::new(TIX_FAULT_ROOT).join(format!(
                "prefix-sink-{}-{}",
                cut.name(),
                Uuid::new_v4()
            ));
            let evidence = root.join("evidence.json");
            let mut value = plan_value(&evidence);
            value["component"] = Value::String("parity_closeout".to_string());
            value["cut"] = Value::String(cut.name().to_string());
            let fault = TerminalFaultPlan::parse(&value).expect("prefix sink fault plan");
            write_durable_new_json(
                &evidence,
                &json!({
                    "schema": EVIDENCE_SCHEMA,
                    "tape_uuid": "71717171-7171-7171-7171-717171717171",
                    "component": "parity_closeout",
                    "cut": cut.name(),
                    "nonce": "91919191-9191-9191-9191-919191919191",
                }),
            )
            .expect("write intentionally mismatched evidence");
            let prefix = terminal_prefix_plan();
            let mut inner = CountingRawSink {
                position: prefix.start_lba,
                ..CountingRawSink::default()
            };
            {
                let mut sink = TerminalPrefixFaultSink::new(&mut inner, Some(&fault), &prefix);
                let block = [0u8; 16];
                let result = if cut == TerminalFaultCut::BeforeFinalParityMap {
                    sink.write_fixed_block(&block).map(|_| ())
                } else {
                    for _ in 0..3 {
                        sink.write_fixed_block(&block)
                            .expect("ParityMap block precedes post-emission cut");
                    }
                    sink.write_filemarks(1, true).map(|_| ())
                };
                assert!(
                    result.is_err(),
                    "mismatched evidence proves cut was reached"
                );
            }
            assert_eq!(inner.blocks, expected_blocks);
            assert_eq!(inner.filemark_calls, expected_filemarks);
            fs::remove_dir_all(root).expect("remove prefix sink evidence");
        }
    }

    #[test]
    fn assignment_race_uses_exact_generation_cas_once() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-tix-assignment-race")
            .tempdir()
            .expect("temp dir");
        let root = Path::new(TIX_FAULT_ROOT).join(format!("contract-{}", Uuid::new_v4()));
        let evidence = root.join("evidence.json");
        let mut value = plan_value(&evidence);
        value["component"] = Value::String("assignment_race".to_string());
        value["cut"] = Value::String("before_assignment_reread".to_string());
        let plan = TerminalFaultPlan::parse(&value).expect("assignment plan");
        let tape_uuid = [0x71; 16];
        let mut index = CatalogIndex::open(temp.path().join("rem-state.sqlite")).expect("index");
        index
            .provision_tape(ProvisionTapeInput {
                tape_uuid,
                voltag: "RMN009L9".to_string(),
                block_size: 1_048_576,
                parity: ParityConfig::None,
                force: false,
            })
            .expect("provision tape");
        index
            .upsert_tape_pool_projection(TapePoolProjectionInput {
                pool_id: "terminal-index".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .expect("project pool");
        let assigned = index
            .compare_and_set_tape_pool_assignment(tape_uuid, 0, Some("terminal-index"))
            .expect("assign tape");
        assert_eq!(assigned.assignment_generation, 1);

        plan.clear_assignment_before_reread(
            &mut index,
            tape_uuid,
            assigned.assignment_generation,
            Some("terminal-index"),
        )
        .expect("inject assignment race");
        let cleared = index
            .get_tape_assignment_snapshot(&tape_uuid)
            .expect("read assignment")
            .expect("known tape");
        assert_eq!(cleared.pool_id, None);
        assert_eq!(cleared.assignment_generation, 2);
        let recorded: Value =
            serde_json::from_slice(&fs::read(&evidence).expect("evidence")).expect("evidence JSON");
        assert_eq!(recorded["assignment_race"]["before_generation"], "1");
        assert_eq!(recorded["assignment_race"]["after_generation"], "2");

        plan.clear_assignment_before_reread(
            &mut index,
            tape_uuid,
            assigned.assignment_generation,
            Some("terminal-index"),
        )
        .expect("existing evidence makes plan one-shot");
        let still_cleared = index
            .get_tape_assignment_snapshot(&tape_uuid)
            .expect("read assignment")
            .expect("known tape");
        assert_eq!(still_cleared.assignment_generation, 2);
        fs::remove_dir_all(root).expect("remove assignment evidence");
    }
}
