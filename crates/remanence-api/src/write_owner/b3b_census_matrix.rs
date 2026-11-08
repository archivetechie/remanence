//! §1.4 command × completion × last-consumer matrix. The fixed-size table is
//! the coverage contract: each path has success/usable/unusable/transport cells.
//! All counters are the existing FailCommand dispatch count and outcome::census.
//! Setup is outside the measured interval; every inline CDB is declared below.
use super::*;
use remanence_library::outcome::{ClassifiedOutcome, Completion, Effect};
use remanence_library::{
    BlockRead, BlockSink, BlockSource, DriveHandleSink, DriveHandleSource, SpaceKind,
};
use remanence_parity::{DriveHandleRawSink, RawTapeSink};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    ReadFixed,
    ReadVariable,
    WriteFixed,
    WritePipeline,
    WriteVariable,
    WriteUnpositioned,
    WfmSync,
    WfmImmediate,
    WfmPipeline,
    WfmZero,
    Locate,
    Space,
    Position,
    ModeSense,
    ModeSelect,
    Tur,
    Load,
    Unload,
    Move,
    Prevent,
    Allow,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathKind {
    Handle,
    PoolAppend,
    ParityRaw,
    Closeout,
    TerminalPrefix,
    Admission,
    Mounted,
    Readiness,
    Restore,
}
const PATHS: [PathKind; 9] = [
    PathKind::Handle,
    PathKind::PoolAppend,
    PathKind::ParityRaw,
    PathKind::Closeout,
    PathKind::TerminalPrefix,
    PathKind::Admission,
    PathKind::Mounted,
    PathKind::Readiness,
    PathKind::Restore,
];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionCase {
    Success,
    Usable,
    Unusable,
    Transport,
}
const COMPLETIONS: [CompletionCase; 4] = [
    CompletionCase::Success,
    CompletionCase::Usable,
    CompletionCase::Unusable,
    CompletionCase::Transport,
];
#[derive(Clone, Copy, Debug)]
enum Cell {
    Run {
        dispatches: usize,
        extra_facts: u64,
        explanation: &'static str,
    },
    Na(&'static str),
}
type Cells = [Cell; 4];
const ONE: Cell = Cell::Run {
    dispatches: 1,
    extra_facts: 0,
    explanation: "one target CDB",
};
const RP: Cell = Cell::Run {
    dispatches: 2,
    extra_facts: 0,
    explanation: "target CDB + inline READ POSITION; one classification for each",
};
const MODE: Cell = Cell::Run {
    dispatches: 2,
    extra_facts: 0,
    explanation: "READ BLOCK LIMITS + target MODE SENSE; one classification for each",
};
const WAIT: Cell = Cell::Run {
    dispatches: 3,
    extra_facts: 0,
    explanation: "initializing TUR + target LOAD + final TUR; one classification for each",
};
const WAIT_REJECT: Cell = Cell::Run {
    dispatches: 2,
    extra_facts: 0,
    explanation: "initializing TUR + failed target LOAD; wait terminates",
};
const MOVE_OK: Cell = Cell::Run {
    dispatches: 3,
    extra_facts: 0,
    explanation: "two READ ELEMENT STATUS preflight CDBs + target MOVE",
};
const MOVE_FAIL: Cell = Cell::Run {
    dispatches: 5,
    extra_facts: 0,
    explanation:
        "two READ ELEMENT STATUS preflight CDBs + target MOVE + two mandatory refresh CDBs",
};
const RAW_SEED: Cell = Cell::Run {
    dispatches: 2,
    extra_facts: 0,
    explanation: "raw sink seeds READ POSITION, then target CDB; one classification for each",
};
const RAW_BARRIER_OK: Cell = Cell::Run {
    dispatches: 3,
    extra_facts: 0,
    explanation: "API raw tracker seeds READ POSITION, target WFM, then barrier READ POSITION",
};
const RAW_REJECTED: Cell = Cell::Run {
    dispatches: 2, extra_facts: 1,
    explanation: "seed + rejected command; S15 retains the enclosing operation fence via one abandoned-write fact",
};
const RAW_BARRIER: Cells = [RAW_BARRIER_OK, RAW_REJECTED, RAW_SEED, RAW_SEED];
const RAW_SINGLE: Cells = [RAW_SEED, RAW_REJECTED, RAW_SEED, RAW_SEED];
const SINGLE: Cells = [ONE, ONE, ONE, ONE];
const PROOF: Cells = [RP, ONE, ONE, ONE];
const MODES: Cells = [MODE, MODE, MODE, MODE];
const LOAD_WAIT: Cells = [WAIT, WAIT_REJECT, WAIT_REJECT, WAIT_REJECT];
const MOVES: Cells = [MOVE_OK, MOVE_FAIL, MOVE_FAIL, MOVE_FAIL];
const NO_RAW: Cells = [Cell::Na("RawTapeSink exposes single variable WRITE and sync/immediate WFM, no batch/pipeline or read API"); 4];
const NO_WRITE: Cells =
    [Cell::Na("admission and restore consume reads/position proofs, never media writes"); 4];
const NO_WAIT: Cells = [Cell::Na("readiness wait dispatches TUR and conditional LOAD only"); 4];
const OWNER: Cells = [Cell::Na(
    "configuration and drive control belong to owner preparation, outside this boundary",
); 4];
const ROBOT: Cells =
    [Cell::Na("MOVE belongs to library placement, not these drive-bound API paths"); 4];
const NO_READ: Cells = [Cell::Na("raw write and terminal-prefix emission do not read records"); 4];
const NO_POSITIONED: Cells = [Cell::Na(
    "RawTapeSink uses unpositioned variable WRITE (separate row), not positioned write_block",
); 4];
const NO_PIPE: Cells = [Cell::Na(
    "closeout emits through RawTapeSink, which has no fixed batch or pipelined WFM entry point",
); 4];

struct Row {
    command: Command,
    paths: [Cells; 9],
}
// Columns: handle | pool append | parity raw | closeout | terminal prefix |
// admission | mounted command | readiness wait | restore read.
// Every cell above expands in the same order: success, usable sense, unusable sense, transport.
static MATRIX: [Row; 21] = [
    Row { command: Command::ReadFixed, paths: [SINGLE, SINGLE, NO_READ, SINGLE, NO_READ, SINGLE, SINGLE, NO_WAIT, SINGLE] },
    Row { command: Command::ReadVariable, paths: [SINGLE, SINGLE, NO_READ, SINGLE, NO_READ, SINGLE, SINGLE, NO_WAIT, SINGLE] },
    Row { command: Command::WriteFixed, paths: [SINGLE, SINGLE, NO_RAW, NO_PIPE, NO_RAW, NO_WRITE, SINGLE, NO_WAIT, NO_WRITE] },
    Row { command: Command::WritePipeline, paths: [SINGLE, SINGLE, NO_RAW, NO_PIPE, NO_RAW, NO_WRITE, SINGLE, NO_WAIT, NO_WRITE] },
    Row { command: Command::WriteVariable, paths: [PROOF, PROOF, NO_POSITIONED, NO_POSITIONED, NO_POSITIONED, NO_WRITE, PROOF, NO_WAIT, NO_WRITE] },
    Row { command: Command::WriteUnpositioned, paths: [SINGLE, [Cell::Na("BlockSink has no unpositioned-write method; pool variable WRITE is the positioned row"); 4], RAW_SINGLE, RAW_SINGLE, RAW_SINGLE, NO_WRITE, SINGLE, NO_WAIT, NO_WRITE] },
    Row { command: Command::WfmSync, paths: [PROOF, PROOF, RAW_BARRIER, RAW_BARRIER, RAW_BARRIER, NO_WRITE, PROOF, NO_WAIT, NO_WRITE] },
    Row { command: Command::WfmImmediate, paths: [SINGLE, SINGLE, RAW_SINGLE, RAW_SINGLE, RAW_SINGLE, NO_WRITE, SINGLE, NO_WAIT, NO_WRITE] },
    Row { command: Command::WfmPipeline, paths: [PROOF, PROOF, NO_RAW, NO_PIPE, NO_RAW, NO_WRITE, PROOF, NO_WAIT, NO_WRITE] },
    Row { command: Command::WfmZero, paths: [PROOF, PROOF, RAW_BARRIER, RAW_BARRIER, RAW_BARRIER, NO_WRITE, PROOF, NO_WAIT, NO_WRITE] },
    Row { command: Command::Locate, paths: [PROOF, PROOF, PROOF, PROOF, PROOF, PROOF, PROOF, NO_WAIT, PROOF] },
    Row { command: Command::Space, paths: [PROOF, PROOF, PROOF, PROOF, PROOF, PROOF, PROOF, NO_WAIT, PROOF] },
    Row { command: Command::Position, paths: [SINGLE, SINGLE, SINGLE, SINGLE, SINGLE, SINGLE, SINGLE, NO_WAIT, SINGLE] },
    Row { command: Command::ModeSense, paths: [MODES, MODES, OWNER, OWNER, OWNER, MODES, MODES, NO_WAIT, MODES] },
    Row { command: Command::ModeSelect, paths: [SINGLE, SINGLE, OWNER, OWNER, OWNER, OWNER, SINGLE, NO_WAIT, SINGLE] },
    Row { command: Command::Tur, paths: [SINGLE, SINGLE, OWNER, OWNER, OWNER, OWNER, SINGLE, SINGLE, SINGLE] },
    Row { command: Command::Load, paths: [SINGLE, SINGLE, OWNER, OWNER, OWNER, OWNER, SINGLE, LOAD_WAIT, SINGLE] },
    Row { command: Command::Unload, paths: [SINGLE, OWNER, OWNER, OWNER, OWNER, OWNER, SINGLE, NO_WAIT, OWNER] },
    Row { command: Command::Move, paths: [MOVES, ROBOT, ROBOT, ROBOT, ROBOT, ROBOT, ROBOT, ROBOT, ROBOT] },
    Row { command: Command::Prevent, paths: [SINGLE, SINGLE, OWNER, OWNER, OWNER, OWNER, SINGLE, NO_WAIT, SINGLE] },
    Row { command: Command::Allow, paths: [SINGLE, SINGLE, OWNER, OWNER, OWNER, OWNER, SINGLE, NO_WAIT, SINGLE] },
];

impl Command {
    fn opcode(self) -> u8 {
        match self {
            Self::ReadFixed | Self::ReadVariable => 8,
            Self::WriteFixed
            | Self::WritePipeline
            | Self::WriteVariable
            | Self::WriteUnpositioned => 10,
            Self::WfmSync | Self::WfmImmediate | Self::WfmPipeline | Self::WfmZero => 0x10,
            Self::Locate => 0x92,
            Self::Space => 0x11,
            Self::Position => 0x34,
            Self::ModeSense => 0x1a,
            Self::ModeSelect => 0x15,
            Self::Tur => 0,
            Self::Load | Self::Unload => 0x1b,
            Self::Move => 0xa5,
            Self::Prevent | Self::Allow => 0x1e,
        }
    }
    fn writing(self) -> bool {
        matches!(self.opcode(), 10 | 0x10)
    }
}
fn usable_sense() -> Vec<u8> {
    let mut s = vec![0; 18];
    s[0] = 0x70;
    s[2] = 5;
    s[7] = 10;
    s[12] = 0x24;
    s
}
fn transport(
    inner: Box<dyn SgTransport>,
    command: Command,
    completion: CompletionCase,
    commands: Arc<Mutex<usize>>,
    readiness: bool,
) -> FailCommand {
    let mut sense = usable_sense();
    if matches!(
        command,
        Command::WriteFixed | Command::WritePipeline | Command::ReadFixed
    ) {
        // Fixed completion needs a valid one-record residual as well as usable
        // sense; otherwise B3a correctly promotes it to unknown completion.
        sense[0] = 0xf0;
        sense[6] = 1;
    }
    FailCommand {
        inner,
        opcode: if completion == CompletionCase::Success {
            255
        } else {
            command.opcode()
        },
        commands,
        successful_writes: 0,
        short_write: false,
        message: (completion == CompletionCase::Transport).then_some("matrix transport failure"),
        injected_sense: Some(if completion == CompletionCase::Unusable {
            vec![0x70, 0, 5]
        } else {
            sense
        }),
        initializing_tur: readiness && command == Command::Load,
    }
}
fn position(lba: u64) -> Vec<u8> {
    let mut p = vec![0; 32];
    p[8..16].copy_from_slice(&lba.to_be_bytes());
    p
}
fn config() -> remanence_library::TapeConfig {
    remanence_library::TapeConfig {
        density_code: None,
        block_size: remanence_library::BlockSize::Fixed { size_bytes: 512 },
        compression: false,
        max_block_size_bytes: 1024 * 1024,
        write_protected: false,
        worm: remanence_library::WormMediaState::NotWorm,
    }
}
// SAFETY: this fixture-only proof is tied to the READ POSITION just observed on
// the exclusive injected handle; no physical admission or real device is claimed.
struct Proof(u64);
#[cfg(test)]
unsafe impl remanence_library::AppendBoundaryProof for Proof {
    fn into_boundary(self) -> ([u8; 16], u32, u64, u64) {
        ([93; 16], 0, 10, self.0)
    }
}
fn open_cell(
    command: Command,
    completion: CompletionCase,
    path: PathKind,
) -> (
    DriveHandle,
    Arc<Mutex<usize>>,
    remanence_library::AppendSession,
) {
    let mut mode = vec![0; 28];
    mode[0] = 27;
    mode[3] = 8;
    mode[10] = 2;
    mode[12] = 15;
    mode[13] = 14;
    let responses = [
        include_bytes!("../../../../fixtures/inquiry/drive1-lto9.bin").to_vec(),
        vec![8, 0x80, 0, 3, b'C', b'9', b'A'],
        position(10),
    ]
    .into_iter()
    .chain(match command {
        Command::ReadFixed | Command::ReadVariable => vec![vec![0; 512]],
        Command::ModeSense => vec![vec![0, 0x10, 0, 0, 0, 1], mode],
        _ if matches!(
            path,
            PathKind::ParityRaw | PathKind::Closeout | PathKind::TerminalPrefix
        ) && matches!(command, Command::WfmSync | Command::WfmZero) =>
        {
            vec![
                position(10),
                position(if command == Command::WfmZero { 10 } else { 11 }),
            ]
        }
        _ => vec![
            position(
                if matches!(
                    command,
                    Command::WriteVariable | Command::WfmSync | Command::WfmPipeline
                ) {
                    11
                } else {
                    10
                }
            );
            4
        ],
    });
    let commands = Arc::new(Mutex::new(0));
    let mut t = transport(
        Box::new(FixtureTransport::new().with_responses(responses)),
        command,
        completion,
        commands.clone(),
        path == PathKind::Readiness,
    );
    // Position failure injection starts after the seed observation.
    if command == Command::Position {
        t.successful_writes = 1;
    }
    let mut drive = DriveHandle::open_standalone_with_transport(
        Path::new("/fixture/c9"),
        Box::new(t),
        &StaticAllowlist::new(["/fixture/c9"]),
    )
    .unwrap();
    drive.position().unwrap();
    let admission = drive
        .install_append_admission(Proof(drive.identity_generation()))
        .unwrap();
    (drive, commands, admission)
}

enum Failure {
    Tape(TapeIoError),
    Parity(ParityError),
    Control(remanence_library::DriveOpError),
}
impl Failure {
    fn outcome(&self) -> &ClassifiedOutcome {
        match self {
            Self::Tape(e) => e.outcome(),
            Self::Parity(ParityError::TapeIo(e)) => e.outcome(),
            Self::Control(e) => e.outcome(),
            Self::Parity(e) => panic!("unexpected non-device fact: {e}"),
        }
    }
    fn into_tape(self) -> TapeIoError {
        match self {
            Self::Tape(e) | Self::Parity(ParityError::TapeIo(e)) => e,
            _ => panic!("expected tape error"),
        }
    }
}
/// Invoke the same library adapters used at the API boundary; failure consumers
/// below are the production persistence/rendering functions, never reclassifiers.
fn issue(command: Command, path: PathKind, drive: &mut DriveHandle) -> Result<(), Failure> {
    use Command::*;
    if matches!(
        path,
        PathKind::ParityRaw | PathKind::TerminalPrefix | PathKind::Closeout
    ) && command.writing()
    {
        let mut inner = DriveHandleRawSink::new(drive);
        let mut attempted = false;
        let mut raw = crate::write_owner::checkpoint::ActivityTrackingRawTapeSink::new(
            &mut inner,
            &mut attempted,
        );
        let result = match command {
            WriteUnpositioned => raw.write_fixed_block(&[0; 512]).map(|_| ()),
            WfmSync => raw.write_filemarks(1, false).map(|_| ()),
            WfmImmediate => raw.write_filemarks(1, true).map(|_| ()),
            WfmZero => raw.write_filemarks(0, false).map(|_| ()),
            _ => panic!("table admits an unsupported raw-write cell"),
        }
        .map_err(Failure::Parity);
        assert!(
            attempted,
            "API raw tracker must reach the target media boundary"
        );
        return result;
    }
    if path == PathKind::PoolAppend && command.writing() && command != WriteUnpositioned {
        let mut sink = DriveHandleSink(drive);
        if command == WriteVariable {
            return crate::pool_write::write_fixed_blocks(&mut sink, 512, &[0; 512])
                .map(|_| ())
                .map_err(|e| match e {
                    crate::PoolWriteError::TapeIo(e) => Failure::Tape(e),
                    e => panic!("unexpected pool producer fact: {e}"),
                });
        }
        return match command {
            WriteFixed => sink.write_block_batch(&[0; 512], 512).map(|_| ()),
            WritePipeline => sink
                .write_block_batch_pipelined(
                    &[0; 512],
                    512,
                    &remanence_scsi::read_write::build_write_fixed_cdb(1),
                )
                .map(|_| ()),
            WriteVariable => sink.write_block(&[0; 512]).map(|_| ()),
            WfmSync => sink.write_filemarks(1).map(|_| ()),
            WfmImmediate => sink.write_filemarks_immediate(1),
            WfmPipeline => sink.write_filemarks_pipelined(1).map(|_| ()),
            WfmZero => sink.write_filemarks(0).map(|_| ()),
            _ => unreachable!(),
        }
        .map_err(Failure::Tape);
    }
    if matches!(
        path,
        PathKind::Restore | PathKind::PoolAppend | PathKind::Closeout | PathKind::Admission
    ) {
        let mut source = DriveHandleSource(drive);
        let result = match command {
            ReadFixed => Some(
                source
                    .read_block_batch(&mut [0; 512], 512, 1, 1)
                    .map(|_| ()),
            ),
            ReadVariable => Some(source.read_block(&mut [0; 512]).map(|_| ())),
            Locate => Some(source.locate(10).map(|_| ())),
            Space => Some(source.space(1, SpaceKind::Blocks).map(|_| ())),
            Position => Some(source.position().map(|_| ())),
            _ => None,
        };
        if let Some(result) = result {
            return result.map_err(Failure::Tape);
        }
    }
    match command {
        ReadFixed => drive
            .read_block_batch(&mut [0; 512], 512, 1, 1)
            .map(|_| ())
            .map_err(Failure::Tape),
        ReadVariable => drive
            .read_block(&mut [0; 512])
            .map(|_| ())
            .map_err(Failure::Tape),
        WriteFixed => drive
            .write_block_batch(&[0; 512], 512)
            .map(|_| ())
            .map_err(Failure::Tape),
        WritePipeline => drive
            .write_block_batch_pipelined(
                &[0; 512],
                512,
                &remanence_scsi::read_write::build_write_fixed_cdb(1),
            )
            .map(|_| ())
            .map_err(Failure::Tape),
        WriteVariable => drive
            .write_block(&[0; 512])
            .map(|_| ())
            .map_err(Failure::Tape),
        WriteUnpositioned => drive
            .write_block_unpositioned(&[0; 512])
            .map(|_| ())
            .map_err(Failure::Tape),
        WfmSync => drive.write_filemarks(1).map(|_| ()).map_err(Failure::Tape),
        WfmImmediate => drive.write_filemarks_immediate(1).map_err(Failure::Tape),
        WfmPipeline => drive
            .write_filemarks_pipelined(1)
            .map(|_| ())
            .map_err(Failure::Tape),
        WfmZero => drive.write_filemarks(0).map(|_| ()).map_err(Failure::Tape),
        Locate => drive.locate(10).map(|_| ()).map_err(Failure::Tape),
        Space => drive
            .space(1, SpaceKind::Blocks)
            .map(|_| ())
            .map_err(Failure::Tape),
        Position => drive.position().map(|_| ()).map_err(Failure::Tape),
        ModeSense => drive.read_config().map(|_| ()).map_err(Failure::Tape),
        ModeSelect => drive.write_config(config()).map_err(Failure::Tape),
        Tur => drive.test_unit_ready().map_err(Failure::Tape),
        Load => drive.load().map_err(Failure::Control),
        Unload => drive.unload().map_err(Failure::Control),
        Prevent => drive.prevent_medium_removal().map_err(Failure::Control),
        Allow => drive.allow_medium_removal().map_err(Failure::Control),
        Move => unreachable!("changer runner"),
    }
}

/// Exercise a real API failure consumer, including its final Status/persistence.
fn consume(
    path: PathKind,
    failure: Failure,
    drive: &mut DriveHandle,
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    selected: &SelectedTape,
) -> Status {
    use crate::closeout_obligation::RetryCauseExt;
    let outcome = failure.outcome().clone();
    if let Failure::Control(error) = failure {
        if path == PathKind::Handle {
            return Status::unavailable(error.to_string());
        }
        return crate::write_owner::restore::attach_outcome_retry(
            error.outcome(),
            Status::unavailable(error.to_string()),
        );
    }
    let error = failure.into_tape();
    match path {
        PathKind::Handle => Status::unavailable(error.to_string()),
        PathKind::PoolAppend | PathKind::Restore => {
            crate::write_owner::restore::status_from_pool_write_error(
                crate::PoolWriteError::TapeIo(error),
            )
        }
        PathKind::Admission => {
            let error = crate::append_admission::AdmissionError::from(error);
            let outcome = crate::append_admission::admission_error_outcome(&error);
            crate::append_admission::daemon_admission_error(
                AuditActor::System,
                index,
                &cfg.terminal_config(),
                selected.tape_uuid,
                error,
                &outcome,
            )
        }
        PathKind::ParityRaw | PathKind::Closeout => {
            let error = ParityError::TapeIo(error);
            let outcome =
                crate::write_owner::checkpoint::write_failure_outcome(drive, 0, Some(&error));
            let status =
                crate::write_owner::restore::status_from_parity_error(&error, error.to_string());
            fence_failed_parity_raw_write(
                index,
                cfg,
                selected,
                if path == PathKind::Closeout {
                    "closeout"
                } else {
                    "append"
                },
                None,
                None,
                &outcome,
                &error.to_string(),
                status,
            )
            .0
        }
        PathKind::TerminalPrefix => {
            // The existing recovery-persistence refusal fixture reaches the
            // complete prefix failure function, including its returned Status.
            let mut lease = remanence_state::authority::fixtures::AuthorityBuilder::new(
                &cfg.checkpoint_journal_dir,
                selected.tape_uuid,
            )
            .build()
            .unwrap()
            .acquire_exclusive()
            .unwrap();
            let spec = crate::write_owner::terminal_types::TerminalFinalizeSpec {
                actor: AuditActor::System,
                tape_uuid: selected.tape_uuid,
                block_size: 512,
                pool_config: None,
                trigger: TerminalFinalizationTrigger::ReachedLowWatermark,
                operation_id: None,
                manual: None,
            };
            let error = ParityError::TapeIo(error);
            let outcome =
                crate::write_owner::checkpoint::write_failure_outcome(drive, 0, Some(&error));
            let status = crate::write_owner::terminal_finalize::terminal_prefix_write_failure(
                index,
                cfg,
                &mut lease,
                &spec,
                selected,
                drive.media_write_dispatch_count() > 0,
                &error,
                &outcome,
            );
            assert!(status.message().contains("persist terminal recovery"));
            status
        }
        PathKind::Mounted => outcome.retry.attach(Status::unavailable(error.to_string())),
        PathKind::Readiness => unreachable!("wait consumes its own outcomes"),
    }
}

fn context(path: PathKind, command: Command) -> OperationContext {
    OperationContext {
        phase: match path {
            PathKind::Admission => Phase::Admission,
            PathKind::Closeout => Phase::Closeout,
            PathKind::TerminalPrefix => Phase::Finalization,
            PathKind::Restore => Phase::Read,
            PathKind::Readiness => Phase::Diagnostic,
            _ if command.writing() => Phase::Append,
            _ => Phase::Read,
        },
        region: if command.writing() {
            Region::AtOrAfterCursor(10)
        } else {
            Region::Committed(10..11)
        },
        mounted: path == PathKind::Mounted,
        ..Default::default()
    }
}

/// All declared commands and all four completions must reach their last consumer.
#[test]
fn b3b_one_classification_per_command_matrix() {
    let mut executed = 0;
    let mut not_applicable = 0;
    for row in &MATRIX {
        for (path, cells) in PATHS.into_iter().zip(row.paths) {
            for (completion, cell) in COMPLETIONS.into_iter().zip(cells) {
                let label = format!("{:?}/{path:?}/{completion:?}", row.command);
                match cell {
                    Cell::Na(reason) => {
                        assert!(!reason.is_empty(), "{label}");
                        not_applicable += 1;
                    }
                    Cell::Run {
                        dispatches,
                        extra_facts,
                        explanation,
                    } => {
                        eprintln!("census {label}: {explanation}");
                        if row.command == Command::Move {
                            run_move(completion, dispatches, extra_facts, &label);
                        } else {
                            run_cell(
                                row.command,
                                path,
                                completion,
                                dispatches,
                                extra_facts,
                                &label,
                            );
                        }
                        executed += 1;
                    }
                }
            }
        }
    }
    assert_eq!(executed + not_applicable, 21 * 9 * 4);
    eprintln!("matrix: {executed} executed cells, {not_applicable} n/a cells");
}

fn assert_counts(
    commands: usize,
    before: (usize, u64),
    dispatches: usize,
    extra: u64,
    label: &str,
) {
    assert_eq!(commands - before.0, dispatches, "{label}: dispatch count");
    assert_eq!(
        census::total() - before.1,
        dispatches as u64 + extra,
        "{label}: classification count at last consumer"
    );
}

fn run_cell(
    command: Command,
    path: PathKind,
    completion: CompletionCase,
    dispatches: usize,
    extra: u64,
    label: &str,
) {
    let (mut drive, commands, _admission) = open_cell(command, completion, path);
    let _context = drive.scoped_outcome_context(context(path, command));
    let typed = crate::tests::TypedRoot::open_with_tapes(&[([93; 16], "MATRIX93L9")]);
    // The readiness cells record an operation naming a drive serial, which the typed runtime
    // accepts only for a drive its library log has observed; `TypedRoot` observes none. Those
    // cells touch no fence and no tape, so they keep a plain catalog (the readiness rows are
    // SQL-only in this test); every other cell runs over the typed root.
    let legacy_root = tempfile::tempdir().unwrap();
    let mut index = if path == PathKind::Readiness {
        remanence_state::projector::open(legacy_root.path().join("state.sqlite"))
            .map(|catalog| catalog.write_handle())
            .unwrap()
    } else {
        typed.catalog().write_handle()
    };
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "C9", 0x100, "C9D", 0x400, 1,
    )));
    let library = open_model_library(world);
    let mut cfg = test_typed_write_owner_config(
        &typed,
        &library,
        library_snapshot_cell(library.library().clone()),
    );
    let selected = SelectedTape {
        standalone_endpoint: None,
        pool_id: "c9".into(),
        tape_uuid: [93; 16],
        block_size: 512,
        parity_config: ParityConfig::None,
    };
    if path == PathKind::Mounted {
        cfg.placement =
            crate::write_owner::ActorPlacement::Mounted(crate::write_owner::MountedDriveContext {
                endpoint: drive.endpoint().durable_key().unwrap().clone(),
                tape_uuid: selected.tape_uuid,
                barcode: None,
                cartridge_generation: None,
                identity_generation: drive.identity_generation(),
                actor: AuditActor::System,
                audit_error: Arc::new(Mutex::new(None)),
                audit_session: Arc::new(Mutex::new(None)),
                direct_replay_fault: None,
            });
    }
    let operation_id = Uuid::new_v4();
    if path == PathKind::Readiness {
        record_unthreaded_readiness_operation(&mut index, "C9", operation_id);
    }
    let before = (*commands.lock().unwrap(), census::total());
    if path == PathKind::Readiness {
        let registry = crate::operations::OperationRegistry::default();
        let handle = registry.register(operation_id, "media_readiness_wait");
        let poll = crate::write_owner::readiness::poll_drive_media_readiness(
            &mut index,
            &mut drive,
            crate::write_owner::readiness::DriveReadinessPoll {
                bay: 0x100,
                operation_id,
                family: remanence_library::MediaFamily::Unknown,
                options: remanence_library::MediaReadinessWaitOptions {
                    wait: command == Command::Load,
                    timeout: std::time::Duration::ZERO,
                    poll_interval: std::time::Duration::ZERO,
                },
                cartridge_in_drive: true,
                phase: "census",
            },
            &handle,
        )
        .unwrap();
        let outcome = poll.readiness.outcome();
        assert_eq!(
            outcome.evidence.opcode,
            Some(
                if command == Command::Load && completion == CompletionCase::Success {
                    0
                } else {
                    command.opcode()
                }
            ),
            "{label}: final wait decision"
        );
        assert_eq!(
            poll.readiness.is_ready(),
            completion == CompletionCase::Success,
            "{label}"
        );
        if matches!(
            completion,
            CompletionCase::Unusable | CompletionCase::Transport
        ) {
            assert_eq!(outcome.completion, Completion::Unknown, "{label}");
        }
        assert!(
            !readiness_transition_rows(index.path(), operation_id).is_empty(),
            "{label}: no persisted wait decision"
        );
        assert_counts(*commands.lock().unwrap(), before, dispatches, extra, label);
        return;
    }
    let mut observed: Option<ClassifiedOutcome> = None;
    let mut operation = |index: &mut CatalogIndex, drive: &mut DriveHandle| {
        let result = issue(command, path, drive);
        assert_eq!(
            result.is_ok(),
            completion == CompletionCase::Success,
            "{label}: completion branch"
        );
        let outcome = match &result {
            Ok(()) => drive.last_command_outcome().expect(label),
            Err(e) => e.outcome(),
        }
        .clone();
        assert_eq!(
            outcome.evidence.opcode,
            Some(command.opcode()),
            "{label}: primary CDB"
        );
        assert_eq!(
            outcome.evidence.failed,
            completion != CompletionCase::Success,
            "{label}"
        );
        if completion == CompletionCase::Usable {
            assert!(
                outcome.evidence.current_sense().is_some(),
                "{label}: {outcome:?}"
            );
        }
        if completion == CompletionCase::Unusable {
            assert!(outcome.evidence.current_sense().is_none(), "{label}");
        }
        assert_eq!(
            outcome.completion,
            if matches!(
                completion,
                CompletionCase::Unusable | CompletionCase::Transport
            ) {
                Completion::Unknown
            } else {
                Completion::Known
            },
            "{label}"
        );
        observed = Some(outcome);
        result.map_err(|e| consume(path, e, drive, index, &cfg, &selected))
    };
    let result = if path == PathKind::Mounted {
        crate::write_owner::mounted_commands::with_evidence(
            &mut index,
            &cfg,
            &mut drive,
            &mut operation,
        )
    } else {
        operation(&mut index, &mut drive)
    };
    if let Err(status) = result {
        if !matches!(path, PathKind::TerminalPrefix | PathKind::Handle) {
            assert!(
                status.metadata().get("remanence-retry").is_some(),
                "{label}: retry metadata"
            );
        }
    }
    let expected_fences = usize::from(
        (matches!(observed.unwrap().effect, Effect::WriteFence(_))
            || (drive.media_write_dispatch_count() > 0
                && completion != CompletionCase::Success
                && matches!(
                    path,
                    PathKind::ParityRaw | PathKind::Closeout | PathKind::TerminalPrefix
                )))
            && matches!(
                path,
                PathKind::ParityRaw
                    | PathKind::Closeout
                    | PathKind::TerminalPrefix
                    | PathKind::Mounted
            ),
    );
    assert_eq!(
        index.list_active_tape_io_fences().unwrap().len(),
        expected_fences,
        "{label}: fence consumer"
    );
    assert_counts(*commands.lock().unwrap(), before, dispatches, extra, label);
}

/// MOVE uses the library handle and its mandatory refresh, with the same scripted
/// failure helper and census as the drive cells; no device paths are opened.
/// ModelTransport supplies endpoint reads only. FailCommand scripts the no-data
/// completion, and the real handle applies its successful inventory update. Each
/// cell opens a fresh world; no later command reads a successful move from it.
fn run_move(completion: CompletionCase, dispatches: usize, extra: u64, label: &str) {
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "C9M", 0x100, "C9D", 0x400, 1,
    )));
    world
        .lock()
        .unwrap()
        .put_tape_in_slot(0x400, "C9M001L9", VirtualTape::default());
    let library = world.lock().unwrap().library_snapshot();
    let policy = StaticAllowlist::new(["C9M"]);
    let commands = Arc::new(Mutex::new(0));
    let captured = commands.clone();
    let mut handle = library
        .open_with(&policy, move |path| {
            let role = world.lock().unwrap().role_for_path(path).unwrap();
            Ok(Box::new(transport(
                Box::new(ModelTransport::new(world.clone(), role)),
                Command::Move,
                completion,
                captured.clone(),
                false,
            )))
        })
        .unwrap();
    let before = (*commands.lock().unwrap(), census::total());
    let result = handle.move_medium(0x400, 0x100, &policy);
    assert_eq!(
        result.is_ok(),
        completion == CompletionCase::Success,
        "{label}"
    );
    if let Err(error) = result {
        assert_eq!(error.outcome().evidence.opcode, Some(0xa5));
        let _ = remanence_library::classify_changer_not_ready(&error);
        let _ = remanence_library::classify_changer_ready_transition(&error);
    }
    assert_counts(*commands.lock().unwrap(), before, dispatches, extra, label);
}
