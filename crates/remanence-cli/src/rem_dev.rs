//! Development-only vectors and scratch-tape drills. Release packages exclude this target.
use clap::{Args, Parser, Subcommand};
use remanence_cli::{parse_binary_byte_count, parse_element_addr, run_state_change};
use remanence_library::{BlockSize, DiscoveryError, DiscoveryReport, TapeConfig};
use std::{
    fs,
    fs::File,
    io::{self, BufReader, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

#[cfg(test)]
#[path = "dev/damage_vectors.rs"]
mod damage_vectors;
#[path = "dev/freeze_drill.rs"]
mod freeze_drill;
#[path = "dev/generate_tape_images.rs"]
mod generate_tape_images;
#[path = "dev/negative_vectors.rs"]
pub mod negative_vectors;
#[path = "dev/resume_vectors.rs"]
pub mod resume_vectors;
#[path = "dev/tape_image.rs"]
pub mod tape_image;
#[path = "dev/tape_image_vectors.rs"]
pub mod tape_image_vectors;
#[path = "dev/terminal_index_drill.rs"]
mod terminal_index_drill;
const DEFAULT_DEV_TAPE_RECORD_BYTES: usize = 1024 * 1024;
const MAX_SCSI_VARIABLE_WRITE_BYTES: usize = 0x00FF_FFFF;

#[derive(Parser, Debug)]
#[command(
    name = "rem-dev",
    version,
    about = "Development vectors and scratch-tape drills; never shipped"
)]
struct DevCli {
    #[arg(long, global = true)]
    allow: Vec<String>,
    #[arg(long, global = true)]
    allow_derived: Vec<String>,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand, Debug)]
enum Command {
    /// Generate or check review vectors with the existing generator arguments.
    GenerateTapeImages {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a mounted scratch-tape drill.
    Tape {
        #[command(subcommand)]
        command: TapeCommand,
    },
    /// Write development fixtures to a scratch tape.
    Dev {
        #[command(subcommand)]
        command: DevCommand,
    },
}
#[derive(Subcommand, Debug)]
enum TapeCommand {
    /// Write and verify a scratch tape with injected read faults.
    FreezeDrill(TapeFreezeDrillArgs),
    /// Exercise terminal-index selection and recovery with injected read faults.
    TerminalIndexDrill(terminal_index_drill::TerminalIndexDrillArgs),
}

fn main() -> ExitCode {
    remanence_api::read_core::set_read_reservoir_warning_sink(|message| {
        let _ = writeln!(io::stderr().lock(), "warning: {message}");
    });
    let cli = DevCli::parse();
    run(
        cli,
        discover,
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

fn discover() -> Result<DiscoveryReport, DiscoveryError> {
    #[cfg(target_os = "linux")]
    {
        remanence_cli::cli_discover()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(DiscoveryError::EnumerationDenied {
            cause: remanence_library::IoErrorKind {
                kind: "Unsupported",
                message: "device discovery is only implemented on Linux".into(),
                raw_os_error: None,
            },
        })
    }
}

fn run<F>(cli: DevCli, discover: F, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode
where
    F: FnOnce() -> Result<DiscoveryReport, DiscoveryError>,
{
    match cli.command {
        Command::GenerateTapeImages { args } => match generate_tape_images::run(args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                ExitCode::from(1)
            }
        },
        Command::Tape { command } => {
            let validation = match &command {
                TapeCommand::FreezeDrill(args) => args.validate_before_discovery(),
                TapeCommand::TerminalIndexDrill(args) => args.validate_before_discovery(),
            };
            if let Err(error) = validation {
                let _ = writeln!(err, "error: {error}");
                return ExitCode::from(1);
            }
            match command {
                TapeCommand::FreezeDrill(args) => freeze_drill::run_live(
                    freeze_drill::LiveDrillRequest {
                        device: &args.device,
                        block_size: args.block_size.bytes(),
                        data_mib: args.data_mib,
                        default_index_separation: args.default_index_separation,
                        damage_plan: args.damage_plan,
                        unreadable_bot_ack: args.yes_i_know_scratch,
                        report_path: &args.report,
                    },
                    out,
                    err,
                ),
                TapeCommand::TerminalIndexDrill(args) => {
                    terminal_index_drill::run_live(&args, out, err)
                }
            }
        }
        Command::Dev { command } => {
            let target = command.tape_target();
            if !cli.allow.iter().any(|serial| serial == target) {
                let _ = writeln!(err, "error: library {target:?} not on the --allow list — state-changing ops are refused");
                let _ = writeln!(
                    err,
                    "       pass `--allow {target}` to permit this invocation"
                );
                return ExitCode::from(1);
            }
            if let Err(error) = command.validate_before_discovery() {
                let _ = writeln!(err, "error: {error}");
                return ExitCode::from(1);
            }
            match discover() {
                Ok(report) => {
                    run_dev_command(&report, &command, &cli.allow, &cli.allow_derived, out, err)
                }
                Err(error) => {
                    remanence_cli::print_discovery_error(&error, err);
                    ExitCode::from(1)
                }
            }
        }
    }
}

#[derive(Args, Debug)]
struct TapeFreezeDrillArgs {
    /// Explicit SG tape-drive device containing the mounted scratch tape.
    #[arg(long, value_name = "/dev/sgN")]
    device: PathBuf,

    /// Fixed tape block size for this drill invocation.
    #[arg(long, value_enum)]
    block_size: freeze_drill::FreezeDrillBlockSize,

    /// Deterministic payload volume in MiB across the drill objects.
    #[arg(long, default_value_t = 512)]
    data_mib: u64,

    /// Use the production default one-GiB separation extent instead of the
    /// compact three-record drill profile.
    #[arg(long)]
    default_index_separation: bool,

    /// Read-side medium-error arrangement to inject.
    #[arg(long, value_enum)]
    damage_plan: freeze_drill::DamagePlan,

    /// Permit a scratch-tape overwrite when BOT cannot be read.
    #[arg(long)]
    yes_i_know_scratch: bool,

    /// Destination for the stable JSON drill report.
    #[arg(long, value_name = "PATH.json")]
    report: PathBuf,
}

impl TapeFreezeDrillArgs {
    fn validate_before_discovery(&self) -> Result<(), String> {
        fs::metadata(&self.device).map_err(|error| {
            format!(
                "inspect freeze-drill device {}: {error}",
                self.device.display()
            )
        })?;
        if self.data_mib == 0 {
            return Err("tape freeze-drill --data-mib must be greater than zero".to_string());
        }
        if self.report == self.device {
            return Err("tape freeze-drill --report must not name the tape device".to_string());
        }
        match fs::metadata(&self.report) {
            Ok(metadata) if !metadata.is_file() => {
                return Err(format!(
                    "tape freeze-drill --report {} exists and is not a regular file",
                    self.report.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "inspect tape freeze-drill report {}: {error}",
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
                    "tape freeze-drill report parent {} is not a directory",
                    parent.display()
                ));
            }
        }
        Ok(())
    }
}

#[derive(Subcommand, Debug)]
enum DevCommand {
    /// Destructively write a raw dump file to the loaded tape in a drive bay.
    WriteDumpToTape {
        /// Path to a raw dump file.
        #[arg(long, value_name = "PATH")]
        dump: PathBuf,

        /// Library serial for the loaded tape.
        #[arg(long, value_name = "SERIAL")]
        tape: String,

        /// Drive bay containing the loaded scratch tape.
        #[arg(long, value_parser = parse_element_addr)]
        bay: u16,

        /// Variable tape record size to write.
        #[arg(
            long,
            value_name = "BYTES",
            value_parser = parse_record_size,
            default_value_t = DEFAULT_DEV_TAPE_RECORD_BYTES
        )]
        record_size: usize,

        /// Required acknowledgement that the loaded tape will be overwritten.
        #[arg(long = "i-understand-this-overwrites-the-loaded-tape")]
        overwrite_ack: bool,
    },
}

impl DevCommand {
    fn tape_target(&self) -> &str {
        match self {
            Self::WriteDumpToTape { tape, .. } => tape,
        }
    }

    fn validate_before_discovery(&self) -> Result<(), String> {
        match self {
            Self::WriteDumpToTape {
                record_size,
                overwrite_ack,
                ..
            } => {
                if !overwrite_ack {
                    return Err(
                        "dev write-dump-to-tape requires --i-understand-this-overwrites-the-loaded-tape"
                            .to_string(),
                    );
                }
                validate_dev_record_size(*record_size)
            }
        }
    }
}

/// Parse a byte count for dev tape record sizing.
/// Accepts plain bytes plus binary suffixes like `1024K` or `1MiB`.
fn parse_record_size(s: &str) -> Result<usize, String> {
    let bytes = parse_binary_byte_count(s, "record size")?;
    if bytes == 0 {
        return Err("record size must be greater than zero".to_string());
    }
    usize::try_from(bytes).map_err(|_| format!("record size {s:?} is too large for this host"))
}

fn run_dev_command(
    report: &DiscoveryReport,
    command: &DevCommand,
    allow: &[String],
    allow_derived: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match command {
        DevCommand::WriteDumpToTape {
            dump,
            tape,
            bay,
            record_size,
            ..
        } => run_state_change(
            report,
            tape,
            allow,
            allow_derived,
            out,
            err,
            |handle, policy| {
                let mut drive = handle
                    .open_drive(*bay, policy)
                    .map_err(|error| error.to_string())?;
                let report = write_dump_to_loaded_tape(&mut drive, dump, *record_size)?;
                Ok(format!(
                    "wrote {} bytes from {} to bay 0x{bay:04x} in {} records; \
                     wrote 1 filemark and rewound",
                    report.bytes_written,
                    dump.display(),
                    report.records_written
                ))
            },
        ),
    }
}

fn validate_dev_record_size(record_size: usize) -> Result<(), String> {
    if record_size == 0 {
        return Err("record size must be greater than zero".to_string());
    }
    if record_size > MAX_SCSI_VARIABLE_WRITE_BYTES {
        return Err(format!(
            "record size {record_size} exceeds WRITE(6) transfer limit {MAX_SCSI_VARIABLE_WRITE_BYTES}"
        ));
    }
    Ok(())
}

struct DevWriteDumpReport {
    bytes_written: u64,
    records_written: u64,
}

fn write_dump_to_loaded_tape(
    drive: &mut remanence_library::DriveHandle,
    dump: &Path,
    record_size: usize,
) -> Result<DevWriteDumpReport, String> {
    validate_dev_record_size(record_size)?;
    let metadata = std::fs::metadata(dump)
        .map_err(|error| format!("read dump metadata {}: {error}", dump.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "dump path {} is not a regular file",
            dump.display()
        ));
    }
    let dump_len = metadata.len();
    if dump_len == 0 {
        return Err("dump file is empty".to_string());
    }
    let identity_generation = drive.identity_generation();
    let current_config = drive.read_config().map_err(|error| error.to_string())?;
    if record_size > current_config.max_block_size_bytes as usize {
        return Err(format!(
            "record size {record_size} exceeds drive-reported max block size {}",
            current_config.max_block_size_bytes
        ));
    }

    let mut reader = BufReader::new(
        File::open(dump).map_err(|error| format!("open dump {}: {error}", dump.display()))?,
    );
    let mut buffer = vec![0; record_size];
    let mut report = DevWriteDumpReport {
        bytes_written: 0,
        records_written: 0,
    };

    drive.rewind().map_err(|error| error.to_string())?;
    drive
        .write_config(TapeConfig {
            density_code: None,
            block_size: BlockSize::Variable,
            compression: false,
            max_block_size_bytes: current_config.max_block_size_bytes,
            write_protected: current_config.write_protected,
            worm: current_config.worm,
        })
        .map_err(|error| error.to_string())?;

    drive
        .bind_identity_generation(identity_generation)
        .map_err(|error| error.to_string())?;
    loop {
        let mut filled = 0usize;
        while filled < buffer.len() {
            let read = reader
                .read(&mut buffer[filled..])
                .map_err(|error| format!("read dump {}: {error}", dump.display()))?;
            if read == 0 {
                break;
            }
            filled += read;
        }
        if filled == 0 {
            break;
        }
        let outcome = drive
            .write_block(&buffer[..filled])
            .map_err(|error| error.to_string())?;
        if outcome.bytes_written as usize != filled {
            return Err(format!(
                "short tape write: wrote {} bytes from {filled}-byte record",
                outcome.bytes_written
            ));
        }
        report.bytes_written += filled as u64;
        report.records_written += 1;
        if outcome.end_of_medium {
            return Err("drive reported end-of-medium while writing dump".to_string());
        }
        if filled < buffer.len() {
            break;
        }
    }

    if report.bytes_written != dump_len {
        return Err(format!(
            "wrote {} bytes but dump length is {dump_len}",
            report.bytes_written
        ));
    }
    let filemark = drive
        .write_filemarks(1)
        .map_err(|error| error.to_string())?;
    if filemark.end_of_medium {
        return Err("drive reported end-of-medium while writing filemark".to_string());
    }
    drive.rewind().map_err(|error| error.to_string())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dump_write_gates_refuse_before_discovery() {
        for (allow, ack, expected) in [
            (false, false, "--allow list"),
            (
                true,
                false,
                "--i-understand-this-overwrites-the-loaded-tape",
            ),
        ] {
            let mut args = vec!["rem-dev"];
            if allow {
                args.extend(["--allow", "TEST_LIBRARY"]);
            }
            args.extend([
                "dev",
                "write-dump-to-tape",
                "--dump",
                "/nonexistent/fixture.bin",
                "--tape",
                "TEST_LIBRARY",
                "--bay",
                "0x0100",
            ]);
            if ack {
                args.push("--i-understand-this-overwrites-the-loaded-tape");
            }
            let mut out = Vec::new();
            let mut err = Vec::new();
            let code = run(
                DevCli::parse_from(args),
                || panic!("discovery precedes admission"),
                &mut out,
                &mut err,
            );
            assert_eq!(code, ExitCode::from(1));
            assert!(out.is_empty());
            assert!(String::from_utf8(err).unwrap().contains(expected));
        }
    }

    #[test]
    fn dev_record_size_parser_accepts_binary_suffixes() {
        assert_eq!(parse_record_size("1M").unwrap(), 1024 * 1024);
        assert_eq!(parse_record_size("1MB").unwrap(), 1024 * 1024);
        assert_eq!(parse_record_size("2KiB").unwrap(), 2048);
        assert_eq!(parse_record_size("2KB").unwrap(), 2048);
        assert_eq!(parse_record_size("4096").unwrap(), 4096);
    }

    #[test]
    fn rem_dev_parses_the_documented_freeze_drill_cli() {
        let cli = <crate::DevCli as clap::Parser>::try_parse_from([
            "rem-dev",
            "tape",
            "freeze-drill",
            "--device",
            "/dev/sg7",
            "--block-size",
            "524288",
            "--data-mib",
            "32",
            "--default-index-separation",
            "--damage-plan",
            "terminal-replica-c",
            "--yes-i-know-scratch",
            "--report",
            "/tmp/freeze-report.json",
        ])
        .expect("documented freeze-drill CLI parses");
        let crate::Command::Tape {
            command: crate::TapeCommand::FreezeDrill(args),
        } = cli.command
        else {
            panic!("expected tape freeze-drill command");
        };
        assert_eq!(args.device, Path::new("/dev/sg7"));
        assert_eq!(args.block_size.bytes(), 524_288);
        assert_eq!(args.data_mib, 32);
        assert!(args.default_index_separation);
        assert_eq!(args.damage_plan, freeze_drill::DamagePlan::TerminalReplicaC);
        assert!(args.yes_i_know_scratch);
        assert_eq!(args.report, Path::new("/tmp/freeze-report.json"));
    }

    #[test]
    fn documented_cli_parses() {
        let cli = <crate::DevCli as clap::Parser>::try_parse_from([
            "rem-dev",
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
        terminal_index_drill::assert_documented_args(&args);
    }
}
