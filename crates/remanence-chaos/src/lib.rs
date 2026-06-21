//! Phase B QuadStor chaos adapter for Remanence.
//!
//! This crate implements a transport-level fault adapter for the Remanence SCSI
//! path. `qschaos` Phase A arms scenarios into SQLite; [`ChaosTransport`] reads
//! that state, wraps any [`SgTransport`], injects the L1a command-level fault
//! set, and appends one JSONL event per intercepted command. It deliberately
//! does not talk to QuadStor or `/dev/sg*`; Phase B tests run over
//! `FixtureTransport`.

use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use remanence_library::transport::{SgTransport, TimeoutClass, TransferOutcome};
use remanence_scsi::ScsiError;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Stateful virtual tape/changer transport for Phase C L1b tests.
pub mod model;

/// Environment variable that enables chaos wrapping when set to a truthy value.
pub const ENV_CHAOS_ENABLED: &str = "REM_CHAOS_ENABLED";

/// Environment variable containing the Phase A SQLite state path.
pub const ENV_CHAOS_STATE: &str = "REM_CHAOS_STATE";

/// Errors raised while constructing a chaos engine from Phase A state.
#[derive(Debug, Error)]
pub enum ChaosError {
    /// `REM_CHAOS_ENABLED` asked for chaos but no state path was provided.
    #[error("{ENV_CHAOS_STATE} is required when {ENV_CHAOS_ENABLED} is enabled")]
    MissingStateEnv,
    /// The SQLite state file could not be read.
    #[error("failed to read chaos state {path}: {source}")]
    StateRead {
        /// State database path.
        path: PathBuf,
        /// Underlying SQLite error.
        source: rusqlite::Error,
    },
    /// A JSON column in the Phase A state database was malformed.
    #[error("invalid JSON in chaos state column {column}: {source}")]
    InvalidJson {
        /// JSON column name.
        column: &'static str,
        /// Underlying JSON parse error.
        source: serde_json::Error,
    },
}

/// Host/device identity attached to events and target matching.
///
/// L1a tests seed this directly. Later phases can populate it from discovery,
/// library state, and changer drive-to-barcode coupling.
#[derive(Debug, Clone, Default)]
pub struct DeviceCtx {
    /// Logical drive id, such as `drive1`.
    pub drive_id: Option<String>,
    /// Logical tape id when known.
    pub tape_id: Option<String>,
    /// Barcode currently loaded in the drive when known.
    pub barcode: Option<String>,
    /// Backend label written to JSONL events.
    pub backend: Option<String>,
}

impl DeviceCtx {
    /// Create an empty device context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a copy with `drive_id` set.
    pub fn with_drive_id(mut self, drive_id: impl Into<String>) -> Self {
        self.drive_id = Some(drive_id.into());
        self
    }

    /// Return a copy with `tape_id` set.
    pub fn with_tape_id(mut self, tape_id: impl Into<String>) -> Self {
        self.tape_id = Some(tape_id.into());
        self
    }

    /// Return a copy with `barcode` set.
    pub fn with_barcode(mut self, barcode: impl Into<String>) -> Self {
        self.barcode = Some(barcode.into());
        self
    }

    /// Return a copy with the backend event label set.
    pub fn with_backend(mut self, backend: impl Into<String>) -> Self {
        self.backend = Some(backend.into());
        self
    }

    fn tape_identity(&self) -> Option<&str> {
        self.tape_id.as_deref().or(self.barcode.as_deref())
    }
}

/// Shared Phase B fault engine loaded from a Phase A SQLite state database.
#[derive(Clone)]
pub struct FaultEngine {
    inner: Arc<Mutex<FaultEngineInner>>,
}

impl std::fmt::Debug for FaultEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaultEngine")
            .field("scenario_id", &self.scenario_id())
            .finish_non_exhaustive()
    }
}

impl FaultEngine {
    /// Load the currently armed scenario and ordered faults from `state_path`.
    pub fn from_state_path(state_path: impl AsRef<Path>) -> Result<Self, ChaosError> {
        let state_path = state_path.as_ref().to_path_buf();
        let conn = Connection::open(&state_path).map_err(|source| ChaosError::StateRead {
            path: state_path.clone(),
            source,
        })?;
        let scenario = load_armed_scenario(&conn, &state_path)?;
        let log_file = open_event_log(&state_path);
        Ok(Self {
            inner: Arc::new(Mutex::new(FaultEngineInner {
                conn,
                log_file,
                scenario,
                fired_once: HashSet::new(),
            })),
        })
    }

    /// Load an engine from `REM_CHAOS_STATE`.
    pub fn from_env() -> Result<Self, ChaosError> {
        let state_path = env::var_os(ENV_CHAOS_STATE).ok_or(ChaosError::MissingStateEnv)?;
        Self::from_state_path(PathBuf::from(state_path))
    }

    /// Return the active scenario id, if one is armed.
    pub fn scenario_id(&self) -> Option<String> {
        self.inner
            .lock()
            .expect("chaos engine lock")
            .scenario
            .as_ref()
            .map(|scenario| scenario.id.clone())
    }

    fn pre_call_decision(
        &self,
        ctx: &DeviceCtx,
        command: &CommandInfo,
        requested_bytes: u64,
    ) -> Option<CheckConditionDecision> {
        let mut inner = self.inner.lock().expect("chaos engine lock");
        let scenario = inner.scenario.clone()?;
        let (fault, sense) = scenario
            .faults
            .iter()
            .filter(|fault| {
                !inner.fired_once.contains(&fault.id)
                    && fault_matches(fault, ctx, command)
                    && !is_mutation_fault(fault)
            })
            .find_map(|fault| {
                sense_for_fault(fault, command, requested_bytes).map(|sense| (fault.clone(), sense))
            })?;
        if is_one_shot_fault(&fault) {
            inner.fired_once.insert(fault.id);
        }
        Some(CheckConditionDecision {
            scenario,
            fault,
            sense,
        })
    }

    fn post_read_mutation(
        &self,
        ctx: &DeviceCtx,
        command: &CommandInfo,
        buf: &mut [u8],
    ) -> Option<MutationDecision> {
        let mut inner = self.inner.lock().expect("chaos engine lock");
        let scenario = inner.scenario.clone()?;
        let (fault, spec) = scenario
            .faults
            .iter()
            .filter(|fault| {
                !inner.fired_once.contains(&fault.id)
                    && fault_matches(fault, ctx, command)
                    && is_mutation_fault(fault)
            })
            .find_map(|fault| mutation_spec_for_fault(fault).map(|spec| (fault.clone(), spec)))?;
        let summary = apply_mutation(&scenario, &fault, command, &spec, buf);
        if is_one_shot_fault(&fault) {
            inner.fired_once.insert(fault.id);
        }
        drop(inner);

        let inserted = self.record_corrupt_range(ctx, command, &scenario, &fault, &summary);
        Some(MutationDecision {
            scenario,
            fault,
            summary,
            corrupt_range_inserted: inserted,
        })
    }

    fn log_event(&self, event: Value) {
        let Some(encoded) = serde_json::to_string(&event).ok() else {
            return;
        };

        let mut inner = self.inner.lock().expect("chaos engine lock");
        if let Some(file) = inner.log_file.as_mut() {
            let _ = writeln!(file, "{encoded}");
            let _ = file.flush();
        }

        let scenario_id = event.get("scenario_id").and_then(Value::as_str);
        let fault_id = event.get("fault_id").and_then(Value::as_i64);
        let catalogue_id = event.get("catalogue_id").and_then(Value::as_str);
        let ts = event.get("ts").and_then(Value::as_str).unwrap_or("");
        let _ = inner.conn.execute(
            r#"
            INSERT INTO events(ts, scenario_id, fault_id, catalogue_id, event_json)
            VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![ts, scenario_id, fault_id, catalogue_id, encoded],
        );
    }

    fn record_corrupt_range(
        &self,
        ctx: &DeviceCtx,
        command: &CommandInfo,
        scenario: &Scenario,
        fault: &Fault,
        summary: &MutationSummary,
    ) -> bool {
        if summary.applied_length == 0 {
            return false;
        }
        let Some(lba) = command.lba else {
            return false;
        };
        let Ok(lba) = i64::try_from(lba) else {
            return false;
        };
        let Ok(offset) = i64::try_from(summary.offset) else {
            return false;
        };
        let Ok(length) = i64::try_from(summary.applied_length) else {
            return false;
        };
        let tape = ctx.tape_identity();
        let drive = ctx.drive_id.as_deref();
        self.inner
            .lock()
            .expect("chaos engine lock")
            .conn
            .execute(
                r#"
                INSERT INTO corrupt_ranges(
                    scenario_id, fault_id, tape_id, drive_id, lba,
                    offset, length, mode, scope, seed
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                "#,
                params![
                    scenario.id,
                    fault.id,
                    tape,
                    drive,
                    lba,
                    offset,
                    length,
                    summary.mode,
                    fault.scope,
                    scenario.seed
                ],
            )
            .is_ok()
    }
}

/// Transport wrapper that injects Phase B chaos faults at the SCSI CDB seam.
#[derive(Debug)]
pub struct ChaosTransport<T> {
    inner: T,
    engine: Option<FaultEngine>,
    ctx: DeviceCtx,
    current_lba: Option<u64>,
}

impl<T> ChaosTransport<T> {
    /// Wrap `inner` without any chaos engine. Calls forward byte-for-byte.
    pub fn disabled(inner: T) -> Self {
        Self {
            inner,
            engine: None,
            ctx: DeviceCtx::default(),
            current_lba: None,
        }
    }

    /// Wrap `inner` with a pre-loaded fault engine and device context.
    pub fn new(inner: T, engine: FaultEngine, ctx: DeviceCtx) -> Self {
        Self {
            inner,
            engine: Some(engine),
            ctx,
            current_lba: None,
        }
    }

    /// Wrap `inner` with an engine loaded from `state_path`.
    pub fn from_state_path(
        inner: T,
        state_path: impl AsRef<Path>,
        ctx: DeviceCtx,
    ) -> Result<Self, ChaosError> {
        Ok(Self::new(
            inner,
            FaultEngine::from_state_path(state_path)?,
            ctx,
        ))
    }

    /// Wrap `inner` from `REM_CHAOS_*`; returns a disabled wrapper when chaos is off.
    pub fn from_env(inner: T, ctx: DeviceCtx) -> Result<Self, ChaosError> {
        if chaos_enabled_from_env() {
            Ok(Self::new(inner, FaultEngine::from_env()?, ctx))
        } else {
            Ok(Self {
                inner,
                engine: None,
                ctx,
                current_lba: None,
            })
        }
    }

    /// Consume the wrapper and return the wrapped transport.
    pub fn into_inner(self) -> T {
        self.inner
    }

    fn command_info(
        &self,
        cdb: &[u8],
        direction: DataDirection,
        requested_bytes: u64,
    ) -> CommandInfo {
        let mut command = CommandInfo::decode(cdb, direction, requested_bytes);
        if matches!(command.operation, "read" | "write") && command.lba.is_none() {
            command.lba = self.current_lba;
        }
        command.lba_before = command.lba.or(self.current_lba);
        command
    }

    fn update_position_after_success(&mut self, command: &CommandInfo) {
        match command.position_update {
            PositionUpdate::Set(lba) => self.current_lba = Some(lba),
            PositionUpdate::Clear => self.current_lba = None,
            PositionUpdate::Relative(delta) => {
                self.current_lba = self.current_lba.map(|lba| apply_relative_lba(lba, delta));
            }
            PositionUpdate::DataTransfer => {
                if let (Some(lba), Some(blocks)) = (command.lba_before, command.transfer_blocks) {
                    self.current_lba = Some(lba.saturating_add(blocks as u64));
                }
            }
            PositionUpdate::None => {}
        }
    }

    fn update_position_after_in_success(
        &mut self,
        command: &CommandInfo,
        buf: &[u8],
        bytes_transferred: usize,
    ) {
        if command.operation == "read_position" {
            if let Some(lba) = read_position_lba(&buf[..bytes_transferred.min(buf.len())]) {
                self.current_lba = Some(lba);
                return;
            }
        }
        if let (Some(lba), Some(blocks)) = (command.lba_before, command.transfer_blocks) {
            self.current_lba = Some(lba.saturating_add(blocks as u64));
        }
    }
}

impl<T: SgTransport> SgTransport for ChaosTransport<T> {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        let command = self.command_info(cdb, DataDirection::In, buf.len() as u64);
        if let Some(engine) = &self.engine {
            if let Some(decision) = engine.pre_call_decision(&self.ctx, &command, buf.len() as u64)
            {
                let event = command_event(CommandEvent {
                    scenario: Some(&decision.scenario),
                    fault: Some(&decision.fault),
                    ctx: &self.ctx,
                    command: &command,
                    inner_called: false,
                    status: "check_condition",
                    returned_bytes: decision.sense.bytes_transferred as u64,
                    sense: Some(&decision.sense),
                    mutation: None,
                    state_delta: Value::Null,
                });
                engine.log_event(event);
                return Err(ScsiError::CheckCondition {
                    sense: decision.sense.to_fixed_sense(),
                    bytes_transferred: decision.sense.bytes_transferred,
                });
            }
        }

        let result = self.inner.execute_in(cdb, buf);
        match result {
            Ok(outcome) => {
                let returned_bytes = outcome.bytes_transferred as usize;
                let mutation = if command.operation == "read" {
                    let cap = returned_bytes.min(buf.len());
                    self.engine.as_ref().and_then(|engine| {
                        engine.post_read_mutation(&self.ctx, &command, &mut buf[..cap])
                    })
                } else {
                    None
                };
                self.update_position_after_in_success(&command, buf, returned_bytes);
                if let Some(engine) = &self.engine {
                    let (scenario, fault, state_delta) = mutation.as_ref().map_or(
                        (None, None, Value::Null),
                        |mutation| {
                            (
                                Some(&mutation.scenario),
                                Some(&mutation.fault),
                                json!({
                                    "corrupt_ranges_inserted": mutation.corrupt_range_inserted as u8
                                }),
                            )
                        },
                    );
                    let active = if scenario.is_none() {
                        active_scenario(engine)
                    } else {
                        None
                    };
                    let event = command_event(CommandEvent {
                        scenario: scenario.or(active.as_ref()),
                        fault,
                        ctx: &self.ctx,
                        command: &command,
                        inner_called: true,
                        status: "good",
                        returned_bytes: outcome.bytes_transferred as u64,
                        sense: None,
                        mutation: mutation.as_ref().map(|mutation| &mutation.summary),
                        state_delta,
                    });
                    engine.log_event(event);
                }
                Ok(outcome)
            }
            Err(err) => {
                if let Some(engine) = &self.engine {
                    let (returned_bytes, sense) = scsi_error_event_shape(&err);
                    let event = command_event(CommandEvent {
                        scenario: active_scenario(engine).as_ref(),
                        fault: None,
                        ctx: &self.ctx,
                        command: &command,
                        inner_called: true,
                        status: "inner_error",
                        returned_bytes,
                        sense: sense.as_ref(),
                        mutation: None,
                        state_delta: Value::Null,
                    });
                    engine.log_event(event);
                }
                Err(err)
            }
        }
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let command = self.command_info(cdb, DataDirection::None, 0);
        if let Some(engine) = &self.engine {
            if let Some(decision) = engine.pre_call_decision(&self.ctx, &command, 0) {
                let event = command_event(CommandEvent {
                    scenario: Some(&decision.scenario),
                    fault: Some(&decision.fault),
                    ctx: &self.ctx,
                    command: &command,
                    inner_called: false,
                    status: "check_condition",
                    returned_bytes: 0,
                    sense: Some(&decision.sense),
                    mutation: None,
                    state_delta: Value::Null,
                });
                engine.log_event(event);
                return Err(ScsiError::CheckCondition {
                    sense: decision.sense.to_fixed_sense(),
                    bytes_transferred: 0,
                });
            }
        }

        let result = self.inner.execute_none(cdb);
        match result {
            Ok(()) => {
                self.update_position_after_success(&command);
                if let Some(engine) = &self.engine {
                    let event = command_event(CommandEvent {
                        scenario: active_scenario(engine).as_ref(),
                        fault: None,
                        ctx: &self.ctx,
                        command: &command,
                        inner_called: true,
                        status: "good",
                        returned_bytes: 0,
                        sense: None,
                        mutation: None,
                        state_delta: Value::Null,
                    });
                    engine.log_event(event);
                }
                Ok(())
            }
            Err(err) => {
                if let Some(engine) = &self.engine {
                    let (returned_bytes, sense) = scsi_error_event_shape(&err);
                    let event = command_event(CommandEvent {
                        scenario: active_scenario(engine).as_ref(),
                        fault: None,
                        ctx: &self.ctx,
                        command: &command,
                        inner_called: true,
                        status: "inner_error",
                        returned_bytes,
                        sense: sense.as_ref(),
                        mutation: None,
                        state_delta: Value::Null,
                    });
                    engine.log_event(event);
                }
                Err(err)
            }
        }
    }

    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        let command = self.command_info(cdb, DataDirection::Out, buf.len() as u64);
        if let Some(engine) = &self.engine {
            if let Some(decision) = engine.pre_call_decision(&self.ctx, &command, buf.len() as u64)
            {
                let event = command_event(CommandEvent {
                    scenario: Some(&decision.scenario),
                    fault: Some(&decision.fault),
                    ctx: &self.ctx,
                    command: &command,
                    inner_called: false,
                    status: "check_condition",
                    returned_bytes: decision.sense.bytes_transferred as u64,
                    sense: Some(&decision.sense),
                    mutation: None,
                    state_delta: Value::Null,
                });
                engine.log_event(event);
                return Err(ScsiError::CheckCondition {
                    sense: decision.sense.to_fixed_sense(),
                    bytes_transferred: decision.sense.bytes_transferred,
                });
            }
        }

        let result = self.inner.execute_out(cdb, buf);
        match result {
            Ok(outcome) => {
                self.update_position_after_success(&command);
                if let Some(engine) = &self.engine {
                    let event = command_event(CommandEvent {
                        scenario: active_scenario(engine).as_ref(),
                        fault: None,
                        ctx: &self.ctx,
                        command: &command,
                        inner_called: true,
                        status: "good",
                        returned_bytes: outcome.bytes_transferred as u64,
                        sense: None,
                        mutation: None,
                        state_delta: Value::Null,
                    });
                    engine.log_event(event);
                }
                Ok(outcome)
            }
            Err(err) => {
                if let Some(engine) = &self.engine {
                    let (returned_bytes, sense) = scsi_error_event_shape(&err);
                    let event = command_event(CommandEvent {
                        scenario: active_scenario(engine).as_ref(),
                        fault: None,
                        ctx: &self.ctx,
                        command: &command,
                        inner_called: true,
                        status: "inner_error",
                        returned_bytes,
                        sense: sense.as_ref(),
                        mutation: None,
                        state_delta: Value::Null,
                    });
                    engine.log_event(event);
                }
                Err(err)
            }
        }
    }

    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

/// Return `inner` untouched when chaos is disabled; otherwise return a
/// `ChaosTransport` boxed as `dyn SgTransport`.
pub fn maybe_wrap_from_env<T>(inner: T, ctx: DeviceCtx) -> Result<Box<dyn SgTransport>, ChaosError>
where
    T: SgTransport + 'static,
{
    if chaos_enabled_from_env() {
        Ok(Box::new(ChaosTransport::new(
            inner,
            FaultEngine::from_env()?,
            ctx,
        )))
    } else {
        Ok(Box::new(inner))
    }
}

/// Return true when `REM_CHAOS_ENABLED` has a truthy value.
pub fn chaos_enabled_from_env() -> bool {
    env::var(ENV_CHAOS_ENABLED)
        .map(|value| {
            matches!(
                value.as_str(),
                "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
            )
        })
        .unwrap_or(false)
}

struct FaultEngineInner {
    conn: Connection,
    log_file: Option<BufWriter<File>>,
    scenario: Option<Scenario>,
    fired_once: HashSet<i64>,
}

#[derive(Debug, Clone)]
struct Scenario {
    id: String,
    seed: String,
    faults: Vec<Fault>,
}

#[derive(Debug, Clone)]
struct Fault {
    id: i64,
    catalogue_id: String,
    target: Value,
    trigger: Value,
    action: Value,
    scope: String,
}

#[derive(Debug, Clone)]
struct CheckConditionDecision {
    scenario: Scenario,
    fault: Fault,
    sense: SenseSpec,
}

#[derive(Debug, Clone)]
struct MutationDecision {
    scenario: Scenario,
    fault: Fault,
    summary: MutationSummary,
    corrupt_range_inserted: bool,
}

#[derive(Debug, Clone, Copy)]
enum DataDirection {
    In,
    None,
    Out,
}

impl DataDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::None => "none",
            Self::Out => "out",
        }
    }
}

#[derive(Debug, Clone)]
struct CommandInfo {
    opcode: u8,
    operation: &'static str,
    direction: DataDirection,
    lba: Option<u64>,
    lba_before: Option<u64>,
    transfer_blocks: Option<u32>,
    position_update: PositionUpdate,
    requested_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionUpdate {
    None,
    DataTransfer,
    Set(u64),
    Relative(i64),
    Clear,
}

impl CommandInfo {
    fn decode(cdb: &[u8], direction: DataDirection, requested_bytes: u64) -> Self {
        let opcode = cdb.first().copied().unwrap_or(0xff);
        let (operation, lba, transfer_blocks, position_update) = match opcode {
            0x00 => ("test_unit_ready", None, None, PositionUpdate::None),
            0x03 => ("request_sense", None, None, PositionUpdate::None),
            0x05 => ("read_block_limits", None, None, PositionUpdate::None),
            0x08 => (
                "read",
                None,
                read_write_6_transfer_blocks(cdb),
                PositionUpdate::DataTransfer,
            ),
            0x0A => (
                "write",
                None,
                read_write_6_transfer_blocks(cdb),
                PositionUpdate::DataTransfer,
            ),
            0x10 => (
                "write_filemarks",
                None,
                None,
                write_filemarks_6_count(cdb)
                    .map(|count| PositionUpdate::Relative(count as i64))
                    .unwrap_or(PositionUpdate::None),
            ),
            0x11 => ("space", None, None, space_6_position_update(cdb)),
            0x12 => ("inquiry", None, None, PositionUpdate::None),
            0x15 | 0x55 => ("mode_select", None, None, PositionUpdate::None),
            0x16 => ("reserve", None, None, PositionUpdate::None),
            0x17 => ("release", None, None, PositionUpdate::None),
            0x1A | 0x5A => ("mode_sense", None, None, PositionUpdate::None),
            0x1B => ("load_unload", None, None, PositionUpdate::Clear),
            0x2B | 0x92 => {
                let lba = locate_lba(cdb);
                (
                    "locate",
                    lba,
                    None,
                    lba.map(PositionUpdate::Set).unwrap_or(PositionUpdate::None),
                )
            }
            0x34 => ("read_position", None, None, PositionUpdate::None),
            0x4D => ("log_sense", None, None, PositionUpdate::None),
            0x5E => ("persistent_reserve_in", None, None, PositionUpdate::None),
            0x5F => ("persistent_reserve_out", None, None, PositionUpdate::None),
            0x91 => ("space", None, None, space_16_position_update(cdb)),
            0xA2 => ("security_protocol_in", None, None, PositionUpdate::None),
            0xA5 => ("move_medium", None, None, PositionUpdate::None),
            0xB5 => ("security_protocol_out", None, None, PositionUpdate::None),
            0xB8 => ("read_element_status", None, None, PositionUpdate::None),
            _ => ("unknown", None, None, PositionUpdate::None),
        };
        Self {
            opcode,
            operation,
            direction,
            lba,
            lba_before: lba,
            transfer_blocks,
            position_update,
            requested_bytes,
        }
    }
}

fn read_write_6_transfer_length(cdb: &[u8]) -> Option<u32> {
    if cdb.len() < 5 {
        return None;
    }
    Some(((cdb[2] as u32) << 16) | ((cdb[3] as u32) << 8) | cdb[4] as u32)
}

fn read_write_6_transfer_blocks(cdb: &[u8]) -> Option<u32> {
    let transfer_len = read_write_6_transfer_length(cdb)?;
    if cdb.get(1).copied().unwrap_or(0) & 0x01 != 0 {
        Some(transfer_len)
    } else if transfer_len == 0 {
        Some(0)
    } else {
        Some(1)
    }
}

fn write_filemarks_6_count(cdb: &[u8]) -> Option<u32> {
    if cdb.len() < 5 {
        return None;
    }
    Some(((cdb[2] as u32) << 16) | ((cdb[3] as u32) << 8) | cdb[4] as u32)
}

fn locate_lba(cdb: &[u8]) -> Option<u64> {
    match cdb.first().copied()? {
        0x2B if cdb.len() >= 7 => Some(u32::from_be_bytes([cdb[3], cdb[4], cdb[5], cdb[6]]) as u64),
        0x92 if cdb.len() >= 12 => Some(u64::from_be_bytes([
            cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9], cdb[10], cdb[11],
        ])),
        _ => None,
    }
}

fn space_6_position_update(cdb: &[u8]) -> PositionUpdate {
    if cdb.len() < 5 {
        return PositionUpdate::None;
    }
    let code = cdb[1] & 0x07;
    if code != 0 {
        return PositionUpdate::Clear;
    }
    PositionUpdate::Relative(sign_extend_24(
        ((cdb[2] as u32) << 16) | ((cdb[3] as u32) << 8) | cdb[4] as u32,
    ) as i64)
}

fn space_16_position_update(cdb: &[u8]) -> PositionUpdate {
    if cdb.len() < 12 {
        return PositionUpdate::None;
    }
    let code = cdb[1] & 0x07;
    if code != 0 {
        return PositionUpdate::Clear;
    }
    PositionUpdate::Relative(i64::from_be_bytes([
        cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9], cdb[10], cdb[11],
    ]))
}

fn sign_extend_24(value: u32) -> i32 {
    if value & 0x0080_0000 != 0 {
        (value | 0xff00_0000) as i32
    } else {
        value as i32
    }
}

fn apply_relative_lba(lba: u64, delta: i64) -> u64 {
    if delta >= 0 {
        lba.saturating_add(delta as u64)
    } else {
        lba.saturating_sub(delta.unsigned_abs())
    }
}

fn read_position_lba(buf: &[u8]) -> Option<u64> {
    if buf.len() < 32 {
        return None;
    }
    Some(u64::from_be_bytes([
        buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
    ]))
}

#[derive(Debug, Clone)]
struct SenseSpec {
    response_code: u8,
    key: u8,
    asc: u8,
    ascq: u8,
    filemark: bool,
    eom: bool,
    ili: bool,
    information: Option<u64>,
    bytes_transferred: u32,
}

impl SenseSpec {
    fn to_fixed_sense(&self) -> Vec<u8> {
        let mut sense = vec![0u8; 32];
        sense[0] = self.response_code & 0x7f;
        if let Some(info) = self.information {
            sense[0] |= 0x80;
            let info = (info.min(u32::MAX as u64) as u32).to_be_bytes();
            sense[3..7].copy_from_slice(&info);
        }
        sense[2] = self.key & 0x0f;
        if self.filemark {
            sense[2] |= 0x80;
        }
        if self.eom {
            sense[2] |= 0x40;
        }
        if self.ili {
            sense[2] |= 0x20;
        }
        sense[7] = 24;
        sense[12] = self.asc;
        sense[13] = self.ascq;
        sense
    }

    fn from_fixed_sense(sense: &[u8], bytes_transferred: u32) -> Option<Self> {
        if sense.len() < 14 {
            return None;
        }
        let response_code = sense[0] & 0x7f;
        if !matches!(response_code, 0x70 | 0x71) {
            return None;
        }
        let information = if sense[0] & 0x80 != 0 {
            Some(u32::from_be_bytes([sense[3], sense[4], sense[5], sense[6]]) as u64)
        } else {
            None
        };
        Some(Self {
            response_code,
            key: sense[2] & 0x0f,
            asc: sense[12],
            ascq: sense[13],
            filemark: sense[2] & 0x80 != 0,
            eom: sense[2] & 0x40 != 0,
            ili: sense[2] & 0x20 != 0,
            information,
            bytes_transferred,
        })
    }

    fn event_json(&self) -> Value {
        json!({
            "response_code": hex_byte(self.response_code),
            "sk": hex_byte(self.key),
            "asc": hex_byte(self.asc),
            "ascq": hex_byte(self.ascq),
            "fm": self.filemark,
            "eom": self.eom,
            "ili": self.ili,
            "information": self.information,
        })
    }
}

#[derive(Debug, Clone)]
struct MutationSpec {
    mode: String,
    offset: usize,
    length: usize,
}

#[derive(Debug, Clone)]
struct MutationSummary {
    mode: String,
    offset: usize,
    requested_length: usize,
    applied_length: usize,
    lba: Option<u64>,
}

impl MutationSummary {
    fn event_json(&self) -> Value {
        json!({
            "mode": self.mode,
            "offset": self.offset,
            "length": self.requested_length,
            "applied_length": self.applied_length,
            "lba": self.lba,
        })
    }
}

struct CommandEvent<'a> {
    scenario: Option<&'a Scenario>,
    fault: Option<&'a Fault>,
    ctx: &'a DeviceCtx,
    command: &'a CommandInfo,
    inner_called: bool,
    status: &'static str,
    returned_bytes: u64,
    sense: Option<&'a SenseSpec>,
    mutation: Option<&'a MutationSummary>,
    state_delta: Value,
}

fn command_event(input: CommandEvent<'_>) -> Value {
    let mut row = Map::new();
    row.insert("ts".to_string(), Value::String(now_rfc3339()));
    row.insert(
        "scenario_id".to_string(),
        input
            .scenario
            .map(|scenario| Value::String(scenario.id.clone()))
            .unwrap_or(Value::Null),
    );
    row.insert(
        "fault_id".to_string(),
        input
            .fault
            .map(|fault| Value::Number(fault.id.into()))
            .unwrap_or(Value::Null),
    );
    row.insert(
        "catalogue_id".to_string(),
        input
            .fault
            .map(|fault| Value::String(fault.catalogue_id.clone()))
            .unwrap_or(Value::Null),
    );
    row.insert(
        "operation".to_string(),
        Value::String(input.command.operation.to_string()),
    );
    row.insert(
        "data_direction".to_string(),
        Value::String(input.command.direction.as_str().to_string()),
    );
    row.insert(
        "cdb_opcode".to_string(),
        Value::String(hex_byte(input.command.opcode)),
    );
    row.insert(
        "tape_id".to_string(),
        input
            .ctx
            .tape_id
            .as_ref()
            .map(|value| Value::String(value.clone()))
            .unwrap_or(Value::Null),
    );
    row.insert(
        "barcode".to_string(),
        input
            .ctx
            .barcode
            .as_ref()
            .map(|value| Value::String(value.clone()))
            .unwrap_or(Value::Null),
    );
    row.insert(
        "drive_id".to_string(),
        input
            .ctx
            .drive_id
            .as_ref()
            .map(|value| Value::String(value.clone()))
            .unwrap_or(Value::Null),
    );
    row.insert(
        "backend".to_string(),
        Value::String(
            input
                .ctx
                .backend
                .clone()
                .unwrap_or_else(|| "fixture".to_string()),
        ),
    );
    row.insert(
        "lba_before".to_string(),
        input
            .command
            .lba_before
            .map_or(Value::Null, |lba| json!(lba)),
    );
    let lba_after = match (
        input.status,
        input.command.lba,
        input.command.transfer_blocks,
    ) {
        ("good", Some(lba), Some(blocks)) => Some(lba.saturating_add(blocks as u64)),
        _ => input.command.lba_before,
    };
    row.insert(
        "lba_after".to_string(),
        lba_after.map_or(Value::Null, |lba| json!(lba)),
    );
    row.insert(
        "requested_bytes".to_string(),
        Value::Number(input.command.requested_bytes.into()),
    );
    row.insert(
        "returned_bytes".to_string(),
        Value::Number(input.returned_bytes.into()),
    );
    row.insert("inner_called".to_string(), Value::Bool(input.inner_called));
    row.insert(
        "status".to_string(),
        Value::String(input.status.to_string()),
    );
    row.insert(
        "sense".to_string(),
        input
            .sense
            .map(SenseSpec::event_json)
            .unwrap_or(Value::Null),
    );
    row.insert("tape_alert".to_string(), Value::Null);
    row.insert(
        "mutation_summary".to_string(),
        input
            .mutation
            .map(MutationSummary::event_json)
            .unwrap_or(Value::Null),
    );
    row.insert("state_delta".to_string(), input.state_delta);
    row.insert(
        "seed".to_string(),
        input
            .scenario
            .map(|scenario| Value::String(scenario.seed.clone()))
            .unwrap_or(Value::Null),
    );
    Value::Object(row)
}

fn load_armed_scenario(
    conn: &Connection,
    state_path: &Path,
) -> Result<Option<Scenario>, ChaosError> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT id, seed FROM scenarios ORDER BY armed_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|source| ChaosError::StateRead {
            path: state_path.to_path_buf(),
            source,
        })?;
    let Some((id, seed)) = row else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare(
            r#"
            SELECT id, catalogue_id, target_json, trigger_json, action_json, scope
            FROM faults
            WHERE scenario_id = ?1
            ORDER BY ordinal
            "#,
        )
        .map_err(|source| ChaosError::StateRead {
            path: state_path.to_path_buf(),
            source,
        })?;
    let faults = stmt
        .query_map(params![id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|source| ChaosError::StateRead {
            path: state_path.to_path_buf(),
            source,
        })?
        .map(|row| {
            let (id, catalogue_id, target_json, trigger_json, action_json, scope) =
                row.map_err(|source| ChaosError::StateRead {
                    path: state_path.to_path_buf(),
                    source,
                })?;
            Ok(Fault {
                id,
                catalogue_id,
                target: parse_json_column("target_json", &target_json)?,
                trigger: parse_json_column("trigger_json", &trigger_json)?,
                action: parse_json_column("action_json", &action_json)?,
                scope,
            })
        })
        .collect::<Result<Vec<_>, ChaosError>>()?;
    Ok(Some(Scenario { id, seed, faults }))
}

fn parse_json_column(column: &'static str, raw: &str) -> Result<Value, ChaosError> {
    serde_json::from_str(raw).map_err(|source| ChaosError::InvalidJson { column, source })
}

fn fault_matches(fault: &Fault, ctx: &DeviceCtx, command: &CommandInfo) -> bool {
    target_matches(&fault.target, ctx) && trigger_matches(&fault.trigger, command)
}

fn target_matches(target: &Value, ctx: &DeviceCtx) -> bool {
    if let Some(drive) = target.get("drive").and_then(Value::as_str) {
        if ctx.drive_id.as_deref() != Some(drive) {
            return false;
        }
    }
    if let Some(tape) = target.get("tape").and_then(Value::as_str) {
        if ctx.tape_id.as_deref() != Some(tape) && ctx.barcode.as_deref() != Some(tape) {
            return false;
        }
    }
    true
}

fn trigger_matches(trigger: &Value, command: &CommandInfo) -> bool {
    let Some(op) = trigger.get("op").and_then(Value::as_str) else {
        return false;
    };
    if !operation_matches(op, command) {
        return false;
    }
    if let Some(expected_lba) = json_u64(trigger.get("lba")) {
        if command.lba != Some(expected_lba) {
            return false;
        }
    }
    if let Some(min_lba) = json_u64(trigger.get("lba_at_least")) {
        if command.lba.is_none_or(|lba| lba < min_lba) {
            return false;
        }
    }
    true
}

fn operation_matches(trigger_op: &str, command: &CommandInfo) -> bool {
    match trigger_op {
        "any" => true,
        "open" => matches!(
            command.operation,
            "test_unit_ready" | "inquiry" | "read_block_limits" | "mode_sense"
        ),
        "tur" => command.operation == "test_unit_ready",
        other => other == command.operation,
    }
}

fn is_mutation_fault(fault: &Fault) -> bool {
    fault.catalogue_id == "MED-05" || fault.action.get("mutate").is_some()
}

fn is_one_shot_fault(fault: &Fault) -> bool {
    matches!(fault.scope.as_str(), "transient" | "initiator") || fault.catalogue_id == "BUS-01"
}

fn sense_for_fault(
    fault: &Fault,
    command: &CommandInfo,
    requested_bytes: u64,
) -> Option<SenseSpec> {
    let action = &fault.action;
    let obj = action
        .get("check_condition")
        .or_else(|| action.get("not_ready"))
        .or_else(|| action.get("unit_attention"));
    let defaults = default_sense_for_catalogue(&fault.catalogue_id)?;
    let action_obj = obj.and_then(Value::as_object);

    let response_code = action_obj
        .and_then(|obj| parse_byte_field(obj.get("rc")))
        .unwrap_or(defaults.response_code);
    let key = action_obj
        .and_then(|obj| parse_byte_field(obj.get("sk")))
        .unwrap_or(defaults.key);
    let asc = action_obj
        .and_then(|obj| parse_byte_field(obj.get("asc")))
        .unwrap_or(defaults.asc);
    let ascq = action_obj
        .and_then(|obj| parse_byte_field(obj.get("ascq")))
        .unwrap_or(defaults.ascq);
    let bytes_transferred = action_obj
        .and_then(|obj| parse_bytes_transferred(obj.get("bytes_transferred"), requested_bytes))
        .unwrap_or_else(|| default_bytes_transferred(&fault.catalogue_id, requested_bytes));
    let information = action_obj
        .and_then(|obj| {
            parse_information(obj.get("information"), requested_bytes, bytes_transferred)
        })
        .or_else(|| default_information(&fault.catalogue_id, requested_bytes, bytes_transferred));

    let filemark = action_obj
        .and_then(|obj| obj.get("fm"))
        .and_then(Value::as_bool)
        .unwrap_or(defaults.filemark);
    let eom = action_obj
        .and_then(|obj| obj.get("eom"))
        .and_then(Value::as_bool)
        .unwrap_or(defaults.eom);
    let ili = action_obj
        .and_then(|obj| obj.get("ili"))
        .and_then(Value::as_bool)
        .unwrap_or(defaults.ili);

    if !sense_operation_allowed(&fault.catalogue_id, command) {
        return None;
    }

    Some(SenseSpec {
        response_code,
        key,
        asc,
        ascq,
        filemark,
        eom,
        ili,
        information,
        bytes_transferred,
    })
}

fn default_sense_for_catalogue(catalogue_id: &str) -> Option<SenseSpec> {
    let (response_code, key, asc, ascq, eom) = match catalogue_id {
        "MED-01" => (0x70, 0x03, 0x11, 0x00, false),
        "EOM-01" => (0x70, 0x00, 0x00, 0x02, true),
        "RDY-02" => (0x70, 0x02, 0x04, 0x00, false),
        "BUS-01" => (0x70, 0x06, 0x29, 0x00, false),
        "HOST-01" => (0x71, 0x03, 0x0c, 0x00, false),
        _ => return None,
    };
    Some(SenseSpec {
        response_code,
        key,
        asc,
        ascq,
        filemark: false,
        eom,
        ili: false,
        information: None,
        bytes_transferred: 0,
    })
}

fn sense_operation_allowed(catalogue_id: &str, command: &CommandInfo) -> bool {
    match catalogue_id {
        "MED-01" => command.operation == "read",
        "EOM-01" => command.operation == "write",
        "RDY-02" => matches!(
            command.operation,
            "test_unit_ready" | "inquiry" | "read_block_limits" | "mode_sense"
        ),
        "BUS-01" => true,
        "HOST-01" => matches!(command.operation, "write" | "write_filemarks"),
        _ => false,
    }
}

fn default_bytes_transferred(catalogue_id: &str, requested_bytes: u64) -> u32 {
    match catalogue_id {
        "EOM-01" if requested_bytes > 0 => u32::try_from(requested_bytes / 2).unwrap_or(u32::MAX),
        _ => 0,
    }
}

fn default_information(
    catalogue_id: &str,
    requested_bytes: u64,
    bytes_transferred: u32,
) -> Option<u64> {
    match catalogue_id {
        "EOM-01" => Some(requested_bytes.saturating_sub(bytes_transferred as u64)),
        _ => None,
    }
}

fn parse_bytes_transferred(value: Option<&Value>, requested_bytes: u64) -> Option<u32> {
    match value? {
        Value::Number(n) => n.as_u64().and_then(|value| u32::try_from(value).ok()),
        Value::String(s) if s == "partial" => {
            Some(u32::try_from(requested_bytes / 2).unwrap_or(u32::MAX))
        }
        Value::String(s) if s == "none" => Some(0),
        Value::String(s) if s == "all" => Some(u32::try_from(requested_bytes).unwrap_or(u32::MAX)),
        _ => None,
    }
}

fn parse_information(
    value: Option<&Value>,
    requested_bytes: u64,
    bytes_transferred: u32,
) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) if s == "residual" => {
            Some(requested_bytes.saturating_sub(bytes_transferred as u64))
        }
        _ => None,
    }
}

fn mutation_spec_for_fault(fault: &Fault) -> Option<MutationSpec> {
    let mutate = fault.action.get("mutate")?.as_object()?;
    let mode = mutate
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("xor")
        .to_string();
    if mode != "xor" {
        return None;
    }
    let offset = json_u64(mutate.get("offset"))
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0);
    let length = json_u64(mutate.get("length"))
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(1);
    Some(MutationSpec {
        mode,
        offset,
        length,
    })
}

fn apply_mutation(
    scenario: &Scenario,
    fault: &Fault,
    command: &CommandInfo,
    spec: &MutationSpec,
    buf: &mut [u8],
) -> MutationSummary {
    let applied_length = spec.length.min(buf.len().saturating_sub(spec.offset));
    if spec.mode == "xor" {
        xor_with_deterministic_mask(
            &mut buf[spec.offset..spec.offset + applied_length],
            &scenario.seed,
            &scenario.id,
            fault.id,
            command.lba,
            spec.offset as u64,
        );
    }
    MutationSummary {
        mode: spec.mode.clone(),
        offset: spec.offset,
        requested_length: spec.length,
        applied_length,
        lba: command.lba,
    }
}

fn xor_with_deterministic_mask(
    buf: &mut [u8],
    seed: &str,
    scenario_id: &str,
    fault_id: i64,
    lba: Option<u64>,
    start_offset: u64,
) {
    let mut cursor = 0usize;
    let mut counter = 0u64;
    while cursor < buf.len() {
        let block = deterministic_mask_block(
            seed,
            scenario_id,
            fault_id,
            lba,
            start_offset.saturating_add(cursor as u64),
            counter,
        );
        for mask in block {
            if cursor == buf.len() {
                break;
            }
            buf[cursor] ^= if mask == 0 { 0xa5 } else { mask };
            cursor += 1;
        }
        counter = counter.saturating_add(1);
    }
}

fn deterministic_mask_block(
    seed: &str,
    scenario_id: &str,
    fault_id: i64,
    lba: Option<u64>,
    offset: u64,
    counter: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(seed.as_bytes());
    hasher.update([0]);
    hasher.update(scenario_id.as_bytes());
    hasher.update([0]);
    hasher.update(fault_id.to_be_bytes());
    hasher.update(lba.unwrap_or(0).to_be_bytes());
    hasher.update(offset.to_be_bytes());
    hasher.update(counter.to_be_bytes());
    hasher.finalize().into()
}

fn open_event_log(state_path: &Path) -> Option<BufWriter<File>> {
    let log_path = event_log_path(state_path);
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .ok()
        .map(BufWriter::new)
}

fn parse_byte_field(value: Option<&Value>) -> Option<u8> {
    match value? {
        Value::Number(n) => n.as_u64().and_then(|value| u8::try_from(value).ok()),
        Value::String(s) => parse_hex_byte(s),
        _ => None,
    }
}

fn parse_hex_byte(raw: &str) -> Option<u8> {
    let trimmed = raw
        .strip_prefix("0x")
        .or_else(|| raw.strip_prefix("0X"))
        .unwrap_or(raw);
    u8::from_str_radix(trimmed, 16).ok()
}

fn json_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok().or_else(|| {
            s.strip_prefix("0x")
                .or_else(|| s.strip_prefix("0X"))
                .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        }),
        _ => None,
    }
}

fn active_scenario(engine: &FaultEngine) -> Option<Scenario> {
    engine
        .inner
        .lock()
        .expect("chaos engine lock")
        .scenario
        .clone()
}

fn scsi_error_event_shape(err: &ScsiError) -> (u64, Option<SenseSpec>) {
    match err {
        ScsiError::CheckCondition {
            sense,
            bytes_transferred,
        } => (
            *bytes_transferred as u64,
            SenseSpec::from_fixed_sense(sense, *bytes_transferred),
        ),
        ScsiError::TransportError { sense, .. } => (0, SenseSpec::from_fixed_sense(sense, 0)),
        _ => (0, None),
    }
}

fn event_log_path(state_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.events.jsonl", state_path.display()))
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

fn hex_byte(value: u8) -> String {
    format!("0x{value:02x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use remanence_library::transport::FixtureTransport;
    use rusqlite::Connection;
    use std::sync::{Mutex as StdMutex, OnceLock};

    #[test]
    fn disabled_forwards_fixture_calls_unchanged() {
        let cdb_in = read6(8);
        let cdb_none = [0x10, 0, 0, 0, 1, 0];
        let cdb_out = write6(4);
        let response = vec![0xde, 0xad, 0xbe, 0xef];
        let payload = [0xca, 0xfe, 0xba, 0xbe];

        let mut direct = FixtureTransport::new().with_responses([response.clone()]);
        let mut direct_buf = [0u8; 8];
        let direct_in = direct.execute_in(&cdb_in, &mut direct_buf).unwrap();
        direct.execute_none(&cdb_none).unwrap();
        let direct_out = direct.execute_out(&cdb_out, &payload).unwrap();

        let inner = FixtureTransport::new().with_responses([response]);
        let mut wrapped = ChaosTransport::disabled(inner);
        let mut wrapped_buf = [0u8; 8];
        let wrapped_in = wrapped.execute_in(&cdb_in, &mut wrapped_buf).unwrap();
        wrapped.execute_none(&cdb_none).unwrap();
        let wrapped_out = wrapped.execute_out(&cdb_out, &payload).unwrap();
        let wrapped_inner = wrapped.into_inner();

        assert_eq!(wrapped_in.bytes_transferred, direct_in.bytes_transferred);
        assert_eq!(wrapped_out.bytes_transferred, direct_out.bytes_transferred);
        assert_eq!(wrapped_buf, direct_buf);
        assert_eq!(wrapped_inner.cdb_log, direct.cdb_log);
    }

    #[test]
    fn from_env_med05_mutates_read_and_logs_event() {
        let _guard = env_guard();
        let temp = tempfile::Builder::new()
            .prefix("remanence-chaos-med05")
            .tempdir()
            .unwrap();
        let state_path = temp.path().join("state.db");
        let conn = create_state(&state_path, "med05-rs-parity-basic", "2026-06-09-med05-001");
        insert_fault(
            &conn,
            "med05-rs-parity-basic",
            "MED-05",
            json!({"tape":"RMN002L9","drive":"drive1"}),
            json!({"op":"read","lba":1234}),
            json!({"status":"good","mutate":{"mode":"xor","offset":2,"length":4}}),
            "tape",
        );
        drop(conn);

        env::set_var(ENV_CHAOS_ENABLED, "1");
        env::set_var(ENV_CHAOS_STATE, &state_path);

        let inner = FixtureTransport::new().with_responses([vec![0x11; 16]]);
        let ctx = DeviceCtx::new()
            .with_drive_id("drive1")
            .with_tape_id("RMN002L9")
            .with_backend("fixture");
        let mut transport = ChaosTransport::from_env(inner, ctx).unwrap();
        transport.execute_none(&locate16(1234)).unwrap();
        let mut buf = [0u8; 16];
        let outcome = transport.execute_in(&read6(16), &mut buf).unwrap();

        assert_eq!(outcome.bytes_transferred, 16);
        assert_eq!(&buf[..2], &[0x11, 0x11]);
        assert_ne!(&buf[2..6], &[0x11; 4]);
        assert_eq!(&buf[6..], &[0x11; 10]);

        let events = read_jsonl(&event_log_path(&state_path));
        assert_eq!(events.len(), 2);
        let event = &events[1];
        assert_eq!(event["scenario_id"], "med05-rs-parity-basic");
        assert_eq!(event["catalogue_id"], "MED-05");
        assert_eq!(event["cdb_opcode"], "0x08");
        assert_eq!(event["lba_before"], 1234);
        assert_eq!(event["seed"], "2026-06-09-med05-001");
        assert_eq!(event["status"], "good");
        assert_eq!(event["inner_called"], true);
        assert_eq!(event["mutation_summary"]["offset"], 2);
        assert_eq!(event["mutation_summary"]["length"], 4);
        assert_eq!(event["mutation_summary"]["applied_length"], 4);

        let conn = Connection::open(&state_path).unwrap();
        let corrupt_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM corrupt_ranges", [], |row| row.get(0))
            .unwrap();
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(corrupt_count, 1);
        assert_eq!(event_count, 2);

        env::remove_var(ENV_CHAOS_ENABLED);
        env::remove_var(ENV_CHAOS_STATE);
    }

    #[test]
    fn med01_read_returns_fixed_check_condition_without_inner_call() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-chaos-med01")
            .tempdir()
            .unwrap();
        let state_path = temp.path().join("state.db");
        let conn = create_state(&state_path, "med01", "seed");
        insert_fault(
            &conn,
            "med01",
            "MED-01",
            json!({"drive":"drive1"}),
            json!({"op":"read","lba":7}),
            json!({"check_condition":{}}),
            "transient",
        );
        drop(conn);
        let mut transport = chaos_fixture(&state_path, DeviceCtx::new().with_drive_id("drive1"));
        transport.execute_none(&locate16(7)).unwrap();
        let mut buf = [0u8; 4];
        let err = transport.execute_in(&read6(4), &mut buf).unwrap_err();
        let ScsiError::CheckCondition {
            sense,
            bytes_transferred,
        } = err
        else {
            panic!("expected CheckCondition");
        };
        assert_eq!(bytes_transferred, 0);
        assert_eq!(sense[0] & 0x7f, 0x70);
        assert_eq!(sense[2] & 0x0f, 0x03);
        assert_eq!(sense[12], 0x11);
        assert_eq!(sense[13], 0x00);
        assert_eq!(transport.into_inner().cdb_log, vec![locate16(7).to_vec()]);
    }

    #[test]
    fn eom01_write_returns_fixed_check_condition_with_residual() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-chaos-eom01")
            .tempdir()
            .unwrap();
        let state_path = temp.path().join("state.db");
        let conn = create_state(&state_path, "eom01", "seed");
        insert_fault(
            &conn,
            "eom01",
            "EOM-01",
            json!({"tape":"RMN004L9"}),
            json!({"op":"write","lba_at_least":2048}),
            json!({"check_condition":{"rc":"70","sk":"00","asc":"00","ascq":"02","eom":true,"information":"residual","bytes_transferred":"partial"}}),
            "tape",
        );
        drop(conn);
        let mut transport = chaos_fixture(&state_path, DeviceCtx::new().with_tape_id("RMN004L9"));
        transport.execute_none(&locate16(2048)).unwrap();
        let payload = vec![0x55; 1024];
        let err = transport.execute_out(&write6(1024), &payload).unwrap_err();
        let ScsiError::CheckCondition {
            sense,
            bytes_transferred,
        } = err
        else {
            panic!("expected CheckCondition");
        };
        assert_eq!(bytes_transferred, 512);
        assert_eq!(sense[0] & 0x7f, 0x70);
        assert_ne!(sense[0] & 0x80, 0);
        assert_ne!(sense[2] & 0x40, 0);
        assert_eq!(sense[2] & 0x0f, 0x00);
        assert_eq!(sense[12], 0x00);
        assert_eq!(sense[13], 0x02);
        assert_eq!(
            u32::from_be_bytes([sense[3], sense[4], sense[5], sense[6]]),
            512
        );
    }

    #[test]
    fn rdy_bus_and_host_faults_emit_fixed_check_condition_shapes() {
        let cases = [
            (
                "rdy02",
                "RDY-02",
                json!({"not_ready":{}}),
                json!({"op":"test_unit_ready"}),
                [0x00, 0, 0, 0, 0, 0],
                0x70,
                0x02,
                0x04,
                0x00,
            ),
            (
                "bus01",
                "BUS-01",
                json!({"unit_attention":{}}),
                json!({"op":"test_unit_ready"}),
                [0x00, 0, 0, 0, 0, 0],
                0x70,
                0x06,
                0x29,
                0x00,
            ),
            (
                "host01",
                "HOST-01",
                json!({"check_condition":{}}),
                json!({"op":"write_filemarks"}),
                [0x10, 0, 0, 0, 1, 0],
                0x71,
                0x03,
                0x0c,
                0x00,
            ),
        ];

        for (scenario_id, catalogue_id, action, trigger, cdb, rc, sk, asc, ascq) in cases {
            let temp = tempfile::Builder::new()
                .prefix(&format!("remanence-chaos-{scenario_id}"))
                .tempdir()
                .unwrap();
            let state_path = temp.path().join("state.db");
            let conn = create_state(&state_path, scenario_id, "seed");
            insert_fault(
                &conn,
                scenario_id,
                catalogue_id,
                json!({"drive":"drive1"}),
                trigger,
                action,
                "transient",
            );
            drop(conn);
            let mut transport =
                chaos_fixture(&state_path, DeviceCtx::new().with_drive_id("drive1"));
            let err = transport.execute_none(&cdb).unwrap_err();
            let ScsiError::CheckCondition { sense, .. } = err else {
                panic!("expected CheckCondition for {catalogue_id}");
            };
            assert_eq!(sense[0] & 0x7f, rc, "{catalogue_id} response code");
            assert_eq!(sense[2] & 0x0f, sk, "{catalogue_id} sense key");
            assert_eq!(sense[12], asc, "{catalogue_id} asc");
            assert_eq!(sense[13], ascq, "{catalogue_id} ascq");
        }
    }

    #[test]
    fn unsupported_matching_fault_does_not_block_later_supported_fault() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-chaos-skip-unsupported")
            .tempdir()
            .unwrap();
        let state_path = temp.path().join("state.db");
        let conn = create_state(&state_path, "cascade", "seed");
        insert_fault(
            &conn,
            "cascade",
            "RES-01",
            json!({"drive":"drive1"}),
            json!({"op":"read","lba":42}),
            json!({"target_status":{"status":"0x18","dirty":false}}),
            "until_reset",
        );
        insert_fault(
            &conn,
            "cascade",
            "MED-01",
            json!({"drive":"drive1"}),
            json!({"op":"read","lba":42}),
            json!({"check_condition":{}}),
            "transient",
        );
        drop(conn);

        let mut transport = chaos_fixture(&state_path, DeviceCtx::new().with_drive_id("drive1"));
        transport.execute_none(&locate16(42)).unwrap();
        let mut buf = [0u8; 4];
        let err = transport.execute_in(&read6(4), &mut buf).unwrap_err();
        let ScsiError::CheckCondition { sense, .. } = err else {
            panic!("expected CheckCondition");
        };
        assert_eq!(sense[2] & 0x0f, 0x03);
        assert_eq!(sense[12], 0x11);

        let events = read_jsonl(&event_log_path(&state_path));
        assert_eq!(events[1]["catalogue_id"], "MED-01");
    }

    #[test]
    fn tape_target_matches_loaded_barcode_even_with_different_tape_id() {
        let ctx = DeviceCtx::new()
            .with_tape_id("logical-id")
            .with_barcode("RMN002L9");
        assert!(target_matches(&json!({"tape":"RMN002L9"}), &ctx));
    }

    #[test]
    fn timeout_class_forwards_to_inner_transport() {
        let inner = TimeoutProbe::default();
        let mut transport = ChaosTransport::disabled(inner);
        transport.set_timeout_for(TimeoutClass::TapeIo);
        transport.set_timeout_for(TimeoutClass::Move);
        let inner = transport.into_inner();
        assert_eq!(
            inner.classes,
            vec![TimeoutClass::TapeIo, TimeoutClass::Move]
        );
    }

    #[test]
    fn read_write_6_decode_uses_ssc_transfer_length_not_disk_lba() {
        let read = CommandInfo::decode(&read6(0x12_34_56), DataDirection::In, 0);
        assert_eq!(read.operation, "read");
        assert_eq!(read.lba, None);
        assert_eq!(read.transfer_blocks, Some(1));

        let fixed_read =
            CommandInfo::decode(&[0x08, 0x01, 0x00, 0x00, 0x07, 0x00], DataDirection::In, 0);
        assert_eq!(fixed_read.lba, None);
        assert_eq!(fixed_read.transfer_blocks, Some(7));

        let write = CommandInfo::decode(&write6(0x10_00), DataDirection::Out, 0);
        assert_eq!(write.operation, "write");
        assert_eq!(write.lba, None);
        assert_eq!(write.transfer_blocks, Some(1));
    }

    #[test]
    fn position_tracking_handles_locate_space_filemarks_and_read_position() {
        let mut transport = ChaosTransport::disabled(FixtureTransport::new());
        transport.execute_none(&locate16(100)).unwrap();
        assert_eq!(transport.current_lba, Some(100));

        transport.execute_none(&space6_blocks(7)).unwrap();
        assert_eq!(transport.current_lba, Some(107));

        transport.execute_none(&space6_blocks(-2)).unwrap();
        assert_eq!(transport.current_lba, Some(105));

        transport
            .execute_none(&[0x10, 0x00, 0x00, 0x00, 0x03, 0x00])
            .unwrap();
        assert_eq!(transport.current_lba, Some(108));

        let mut rp = [0u8; 32];
        rp[8..16].copy_from_slice(&333u64.to_be_bytes());
        transport.inner.push_response(rp.to_vec());
        let mut buf = [0u8; 32];
        transport
            .execute_in(&[0x34, 0x06, 0, 0, 0, 0, 0, 0, 0, 0], &mut buf)
            .unwrap();
        assert_eq!(transport.current_lba, Some(333));
    }

    fn chaos_fixture(state_path: &Path, ctx: DeviceCtx) -> ChaosTransport<FixtureTransport> {
        ChaosTransport::from_state_path(FixtureTransport::new(), state_path, ctx).unwrap()
    }

    fn create_state(path: &Path, scenario_id: &str, seed: &str) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE scenarios (
                id TEXT PRIMARY KEY,
                seed TEXT NOT NULL,
                time_scale REAL NOT NULL DEFAULT 1.0,
                source_path TEXT,
                source_json TEXT NOT NULL,
                armed_at TEXT NOT NULL DEFAULT '2026-06-09T00:00:00.000Z'
            );
            CREATE TABLE faults (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scenario_id TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                catalogue_id TEXT NOT NULL,
                target_json TEXT NOT NULL,
                trigger_json TEXT NOT NULL,
                action_json TEXT NOT NULL,
                scope TEXT NOT NULL
            );
            CREATE TABLE corrupt_ranges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scenario_id TEXT NOT NULL,
                fault_id INTEGER,
                tape_id TEXT,
                drive_id TEXT,
                lba INTEGER,
                offset INTEGER NOT NULL,
                length INTEGER NOT NULL,
                mode TEXT NOT NULL,
                scope TEXT NOT NULL,
                seed TEXT,
                created_at TEXT NOT NULL DEFAULT '2026-06-09T00:00:00.000Z'
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL,
                scenario_id TEXT,
                fault_id INTEGER,
                catalogue_id TEXT,
                event_json TEXT NOT NULL
            );
            "#,
        )
        .unwrap();
        conn.execute(
            r#"
            INSERT INTO scenarios(id, seed, source_json)
            VALUES (?1, ?2, '{}')
            "#,
            params![scenario_id, seed],
        )
        .unwrap();
        conn
    }

    fn insert_fault(
        conn: &Connection,
        scenario_id: &str,
        catalogue_id: &str,
        target: Value,
        trigger: Value,
        action: Value,
        scope: &str,
    ) {
        let ordinal: i64 = conn
            .query_row("SELECT COUNT(*) FROM faults", [], |row| row.get(0))
            .unwrap();
        conn.execute(
            r#"
            INSERT INTO faults(
                scenario_id, ordinal, catalogue_id, target_json,
                trigger_json, action_json, scope
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                scenario_id,
                ordinal,
                catalogue_id,
                target.to_string(),
                trigger.to_string(),
                action.to_string(),
                scope
            ],
        )
        .unwrap();
    }

    fn read_jsonl(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn read6(len_bytes: u32) -> [u8; 6] {
        let len = len_bytes.to_be_bytes();
        [0x08, 0x00, len[1], len[2], len[3], 0x00]
    }

    fn write6(len_bytes: u32) -> [u8; 6] {
        let len = len_bytes.to_be_bytes();
        [0x0a, 0x00, len[1], len[2], len[3], 0x00]
    }

    fn locate16(lba: u64) -> [u8; 16] {
        let lba = lba.to_be_bytes();
        [
            0x92, 0x00, 0x00, 0x00, lba[0], lba[1], lba[2], lba[3], lba[4], lba[5], lba[6], lba[7],
            0x00, 0x00, 0x00, 0x00,
        ]
    }

    fn space6_blocks(count: i32) -> [u8; 6] {
        let encoded = ((count as u32) & 0x00ff_ffff).to_be_bytes();
        [0x11, 0x00, encoded[1], encoded[2], encoded[3], 0x00]
    }

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        ENV_LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .expect("env lock")
    }

    #[derive(Debug, Default)]
    struct TimeoutProbe {
        classes: Vec<TimeoutClass>,
    }

    impl SgTransport for TimeoutProbe {
        fn execute_in(
            &mut self,
            _cdb: &[u8],
            _buf: &mut [u8],
        ) -> Result<TransferOutcome, ScsiError> {
            Ok(TransferOutcome::clean(0))
        }

        fn execute_none(&mut self, _cdb: &[u8]) -> Result<(), ScsiError> {
            Ok(())
        }

        fn execute_out(&mut self, _cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
            Ok(TransferOutcome::clean(buf.len() as u32))
        }

        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.classes.push(class);
        }
    }
}
