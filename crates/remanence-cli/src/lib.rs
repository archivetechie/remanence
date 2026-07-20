//! `rem` — the operator-facing CLI for Remanence.
//!
//! Read-only surface (works on any host, Linux-only at the SG_IO
//! level):
//!
//! ```text
//! rem libraries                       # list every library on the host
//! rem library <serial>                # focused view of one library
//! rem library <serial> --slots        # plus per-slot detail
//! rem archive probe --format bru --dump <path.bru>
//! rem archive scan --format bru --dump <path.bru>
//! rem archive restore --format bru --dump <path.bru> --dest <dir>
//! rem archive recover --format bru --dump <path.bru> --dest <dir>
//! ```
//!
//! Break-glass direct hardware surface:
//!
//! ```text
//! rem-debug move    <serial> --src 0x0400 --dst 0x0100
//! rem-debug load    <serial> --slot 0x0400 --bay 0x0100
//! rem-debug unload  <serial> --bay 0x0100 [--dest 0x0400]
//! rem-debug export  <serial> --slot 0x0400
//! rem-debug import  <serial> --slot 0x0400
//! rem-debug rescan  <serial>
//! rem-debug lock    <serial>      # PREVENT MEDIUM REMOVAL
//! rem-debug unlock  <serial>      # ALLOW MEDIUM REMOVAL
//! rem-debug archive scan --format bru --tape <serial> --bay 0x0100 --rewind
//! rem-debug dev write-dump-to-tape --dump <path.bru> --tape <serial> --bay 0x0100 \
//!   --i-understand-this-overwrites-the-loaded-tape
//! ```
//!
//! Direct hardware subcommands require an explicit `--allow <serial>`
//! flag for each library they may target. `--allow-derived <serial>`
//! opts the library into topology-derived drive identities
//! (default-denied). Both are global flags — they apply to the whole
//! invocation.
//!
//! Discovery warnings always print to stderr after the main output.
//! Exit codes: 0 on success, 1 on a discovery / op error, 2 when the
//! caller asked for a library that wasn't found.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Cursor, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration as StdDuration, Instant as StdInstant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use remanence_aead::{
    header::object_id_field, inspect_bytes, EnvelopeSealOptions, RaoHeader, RecipientPublicKey,
    SealOptions, RAO_HEADER_LEN,
};
use remanence_api::pb;
#[cfg(feature = "foreign-bru")]
use remanence_bru::{BruFormat, BRU_BLOCK_SIZE};
#[cfg(all(target_os = "linux", feature = "foreign-bru"))]
use remanence_format::ForeignTapeFormat;
use remanence_format::{
    read_rem_tar_object, write_rem_tar_object_from_readers, ArchiveGapCause, ArchiveGapRange,
    ArchiveReader, BodyLba, DamageRange, DamageStatus, EntryKind, FormatError, ProbeConfidence,
    ProbeResult, RemTarEntryType, RemTarFileLayout, RemTarFileSpec, RemTarFileStream,
    RemTarObjectLayout, RemTarObjectOptions, RemTarReadObject, SourceRequirement, FORMAT_ID,
    MANIFEST_PATH,
};
#[cfg(target_os = "linux")]
use remanence_library::DriveHandlePhysicalSource;
use remanence_library::{
    tape_alert_flag_name, BlockSize, DirtyCause, DiscoveryError, DiscoveryReport, DiscoveryWarning,
    DriveBay, DriveHandle, DriveHandleSink, DriveHandleSource, ElementException, FileBlockSink,
    FileBlockSource, IePort, IoErrorKind, Library, MediaFamily, MediaReadiness,
    MediaReadinessWaitEvent, MediaReadinessWaitOptions, OpenError, Slot, StaticAllowlist,
    TapeAlerts, TapeConfig, VecBlockSource, WormMediaState,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tonic::transport::Channel;
use uuid::Uuid;
use zeroize::Zeroize;

mod archive_ingest;
mod archive_map;
mod pool_ops;
#[cfg(feature = "tui")]
mod top;

const DEFAULT_DAEMON_ENDPOINT: &str = "unix:/var/lib/rem/rem.sock";
const DEFAULT_DEV_TAPE_RECORD_BYTES: usize = 1024 * 1024;
const MAX_SCSI_VARIABLE_WRITE_BYTES: usize = 0x00FF_FFFF;
const MEDIA_CONDITIONING_TIMEOUT: StdDuration = StdDuration::from_secs(9_000);
const MEDIA_CONDITIONING_STEADY_POLL: StdDuration = StdDuration::from_secs(60);
#[cfg(test)]
const CONDITIONAL_LOAD_SETTLE: StdDuration = StdDuration::from_millis(0);
#[cfg(not(test))]
const CONDITIONAL_LOAD_SETTLE: StdDuration = StdDuration::from_secs(1);
#[cfg(not(feature = "foreign-bru"))]
const BRU_BLOCK_SIZE: usize = 2048;

#[cfg(target_os = "linux")]
type CliTransportFactory =
    Box<dyn FnMut(&Path) -> Result<Box<dyn remanence_library::SgTransport>, IoErrorKind>>;

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum CliTransportAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Parser, Debug)]
#[command(name = "rem", version, about = "Remanence — tape library operator CLI")]
struct Cli {
    /// Compatibility allowlist for legacy direct-hardware invocations.
    /// Normal `rem` commands ignore this flag; use `rem-debug --allow`
    /// for break-glass local SCSI/debug work.
    /// May be specified multiple times.
    #[arg(long = "allow", value_name = "SERIAL", global = true, hide = true)]
    allow: Vec<String>,

    /// Compatibility opt-in for topology-derived drive identities on
    /// legacy direct-hardware invocations. Normal `rem` commands ignore
    /// this flag.
    #[arg(
        long = "allow-derived",
        value_name = "SERIAL",
        global = true,
        hide = true
    )]
    allow_derived: Vec<String>,

    #[command(subcommand)]
    command: RemCommand,
}

#[derive(Parser, Debug)]
#[command(
    name = "rem-debug",
    version,
    about = "Break-glass direct SCSI/debug CLI for Remanence"
)]
struct DebugCli {
    /// Library serial(s) this invocation may operate against.
    /// Required for every state-changing subcommand.
    /// May be specified multiple times.
    #[arg(long = "allow", value_name = "SERIAL", global = true)]
    allow: Vec<String>,

    /// Library serial(s) whose `IdentitySource::Derived` drive bays
    /// are also permitted. Subset of `--allow`.
    #[arg(long = "allow-derived", value_name = "SERIAL", global = true)]
    allow_derived: Vec<String>,

    #[command(subcommand)]
    command: Command,
}

struct ParsedCli {
    allow: Vec<String>,
    allow_derived: Vec<String>,
    command: Command,
}

impl From<Cli> for ParsedCli {
    fn from(value: Cli) -> Self {
        Self {
            allow: value.allow,
            allow_derived: value.allow_derived,
            command: value.command.into(),
        }
    }
}

impl From<DebugCli> for ParsedCli {
    fn from(value: DebugCli) -> Self {
        Self {
            allow: value.allow,
            allow_derived: value.allow_derived,
            command: value.command,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CliMode {
    Rem,
    Debug,
}

fn rem_debug_only_reason(cmd: &Command) -> Option<&'static str> {
    match cmd {
        Command::Move { .. }
        | Command::Load { .. }
        | Command::Unload { .. }
        | Command::Export { .. }
        | Command::Import { .. }
        | Command::Rescan { .. }
        | Command::Lock { .. }
        | Command::Unlock { .. } => Some("direct local SCSI mutation"),
        Command::Dev { .. } => Some("development direct tape helper"),
        Command::Archive { command } if command.tape_target().is_some() => {
            Some("direct local tape archive access")
        }
        Command::Libraries { .. }
        | Command::Library { .. }
        | Command::Watch { .. }
        | Command::RebuildCatalogFromJournals { .. }
        | Command::Catalog { .. }
        | Command::Archive { .. }
        | Command::DaemonClient { .. }
        | Command::OperationClient { .. }
        | Command::CatalogClient { .. }
        | Command::AuditClient { .. }
        | Command::DriveClient { .. }
        | Command::AlarmsClient { .. }
        | Command::Top { .. }
        | Command::TapeAlertsAlias { .. }
        | Command::ArchiveVerifyClient { .. }
        | Command::Tape { .. } => None,
    }
}

fn rem_only_reason(cmd: &Command) -> Option<&'static str> {
    match cmd {
        Command::DaemonClient { .. }
        | Command::OperationClient { .. }
        | Command::CatalogClient { .. }
        | Command::AuditClient { .. }
        | Command::DriveClient { .. }
        | Command::AlarmsClient { .. }
        | Command::Top { .. }
        | Command::TapeAlertsAlias { .. }
        | Command::ArchiveVerifyClient { .. } => Some("daemon client commands"),
        Command::Libraries { .. }
        | Command::Library { .. }
        | Command::Move { .. }
        | Command::Load { .. }
        | Command::Unload { .. }
        | Command::Export { .. }
        | Command::Import { .. }
        | Command::Rescan { .. }
        | Command::Lock { .. }
        | Command::Unlock { .. }
        | Command::Watch { .. }
        | Command::RebuildCatalogFromJournals { .. }
        | Command::Catalog { .. }
        | Command::Archive { .. }
        | Command::Tape { .. }
        | Command::Dev { .. } => None,
    }
}

#[derive(Subcommand, Debug)]
enum RemCommand {
    /// List every library `discover()` found on this host.
    #[command(alias = "libs")]
    Libraries {
        /// Emit machine-readable JSON for bringup/orchestrator scripts.
        #[arg(long)]
        json: bool,
    },

    /// Focused view of a single library, by serial.
    #[command(alias = "lib")]
    Library {
        /// Library serial (VPD 0x80 of the medium changer). Run
        /// `rem libraries` to see the available serials.
        serial: String,
        /// Also list every storage slot's status (full/empty + voltag).
        #[arg(long)]
        slots: bool,
        /// Emit machine-readable JSON with drives, slots, and loaded barcodes.
        #[arg(long)]
        json: bool,
    },

    /// Move a cartridge between two element addresses.
    #[command(hide = true)]
    Move {
        /// Library serial.
        serial: String,
        /// Source element address (slot, IE port, or drive bay).
        #[arg(long, value_parser = parse_element_addr)]
        src: u16,
        /// Destination element address.
        #[arg(long, value_parser = parse_element_addr)]
        dst: u16,
    },

    /// Load a cartridge from a slot into a drive bay.
    #[command(hide = true)]
    Load {
        /// Library serial.
        serial: String,
        /// Source storage slot.
        #[arg(long, value_parser = parse_element_addr)]
        slot: u16,
        /// Destination drive bay.
        #[arg(long, value_parser = parse_element_addr)]
        bay: u16,
    },

    /// Unload a cartridge from a drive bay.
    #[command(hide = true)]
    Unload {
        /// Library serial.
        serial: String,
        /// Drive bay to unload.
        #[arg(long, value_parser = parse_element_addr)]
        bay: u16,
        /// Destination slot.
        #[arg(long, value_parser = parse_element_addr)]
        dest: Option<u16>,
    },

    /// Export a cartridge from a slot to the first available IE port.
    #[command(hide = true)]
    Export {
        /// Library serial.
        serial: String,
        /// Source slot.
        #[arg(long, value_parser = parse_element_addr)]
        slot: u16,
    },

    /// Import a cartridge from the first occupied IE port to a slot.
    #[command(hide = true)]
    Import {
        /// Library serial.
        serial: String,
        /// Destination slot.
        #[arg(long, value_parser = parse_element_addr)]
        slot: u16,
    },

    /// INITIALIZE ELEMENT STATUS on the changer, then reconcile.
    #[command(hide = true)]
    Rescan {
        /// Library serial.
        serial: String,
    },

    /// PREVENT MEDIUM REMOVAL on the changer.
    #[command(hide = true)]
    Lock {
        /// Library serial.
        serial: String,
    },

    /// ALLOW MEDIUM REMOVAL on the changer.
    #[command(hide = true)]
    Unlock {
        /// Library serial.
        serial: String,
    },

    /// Subscribe to OS hot-plug events touching SCSI tape / changer subsystems.
    Watch {
        /// Sliding coalescing window.
        #[arg(long, value_name = "DURATION", default_value = "500ms")]
        coalesce_window: String,
    },

    /// Rebuild the local SQLite catalog projection from audit and tape journals.
    RebuildCatalogFromJournals {
        /// Path to `/etc/rem/config.toml`.
        #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
        config: PathBuf,
    },

    /// Query daemon process health and version over Layer 5 gRPC.
    Daemon {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Daemon command to run.
        #[command(subcommand)]
        command: DaemonClientCommand,
    },

    /// Query daemon operation status.
    #[command(name = "op", alias = "operation")]
    Operation {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Operation command to run.
        #[command(subcommand)]
        command: OperationClientCommand,
    },

    /// Query the daemon catalog.
    Catalog {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Catalog command to run.
        #[command(subcommand)]
        command: CatalogClientCommand,
    },

    /// Query the daemon's append-only audit log.
    Audit {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit one JSON object per audit entry.
        #[arg(long, global = true)]
        json: bool,
        /// Audit command to run.
        #[command(subcommand)]
        command: AuditClientCommand,
    },

    /// Query and mutate daemon drive stewardship state.
    Drive {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Drive command to run.
        #[command(subcommand)]
        command: DriveClientCommand,
    },

    /// List or acknowledge standing daemon alarms.
    Alarms {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Include cleared alarms.
        #[arg(long = "all")]
        all: bool,
        /// Alarm command to run. Omitted means list alarms.
        #[command(subcommand)]
        command: Option<AlarmsClientCommand>,
    },

    /// Live top view over daemon state.
    Top {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable JSON instead of the TUI.
        #[arg(long, global = true)]
        json: bool,
        /// Fetch one live-status snapshot and exit.
        #[arg(long, global = true)]
        once: bool,
    },

    /// Initialize tapes after the destructive-safety gauntlet.
    Tape {
        /// Tape operation to run.
        #[command(subcommand)]
        command: RemTapeCommand,
    },

    /// Probe, catalog, or restore a dump archive through a format driver.
    Archive {
        /// Archive operation to run.
        #[command(subcommand)]
        command: RemArchiveCommand,
    },

    /// Restore a native RAO object into a directory.
    Restore(RemArchiveExtractArgs),

    /// Development-only hardware helpers.
    #[command(hide = true)]
    Dev {
        /// Development operation to run.
        #[command(subcommand)]
        command: DevCommand,
    },
}

impl From<RemCommand> for Command {
    fn from(value: RemCommand) -> Self {
        match value {
            RemCommand::Libraries { json } => Self::Libraries { json },
            RemCommand::Library {
                serial,
                slots,
                json,
            } => Self::Library {
                serial,
                slots,
                json,
            },
            RemCommand::Move { serial, src, dst } => Self::Move { serial, src, dst },
            RemCommand::Load { serial, slot, bay } => Self::Load { serial, slot, bay },
            RemCommand::Unload { serial, bay, dest } => Self::Unload { serial, bay, dest },
            RemCommand::Export { serial, slot } => Self::Export { serial, slot },
            RemCommand::Import { serial, slot } => Self::Import { serial, slot },
            RemCommand::Rescan { serial } => Self::Rescan { serial },
            RemCommand::Lock { serial } => Self::Lock { serial },
            RemCommand::Unlock { serial } => Self::Unlock { serial },
            RemCommand::Watch { coalesce_window } => Self::Watch { coalesce_window },
            RemCommand::RebuildCatalogFromJournals { config } => {
                Self::RebuildCatalogFromJournals { config }
            }
            RemCommand::Daemon {
                endpoint,
                json,
                command,
            } => Self::DaemonClient {
                endpoint,
                json,
                command,
            },
            RemCommand::Operation {
                endpoint,
                json,
                command,
            } => Self::OperationClient {
                endpoint,
                json,
                command,
            },
            RemCommand::Catalog {
                endpoint,
                json,
                command,
            } => Self::CatalogClient {
                endpoint,
                json,
                command,
            },
            RemCommand::Audit {
                endpoint,
                json,
                command,
            } => Self::AuditClient {
                endpoint,
                json,
                command,
            },
            RemCommand::Drive {
                endpoint,
                json,
                command,
            } => Self::DriveClient {
                endpoint,
                json,
                command,
            },
            RemCommand::Alarms {
                endpoint,
                json,
                all,
                command,
            } => Self::AlarmsClient {
                endpoint,
                json,
                all,
                command,
            },
            RemCommand::Top {
                endpoint,
                json,
                once,
            } => Self::Top {
                endpoint,
                json,
                once,
            },
            RemCommand::Tape { command } => match command {
                RemTapeCommand::Alerts(args) => Self::TapeAlertsAlias {
                    endpoint: args.endpoint,
                    json: args.json,
                    drive: args.drive,
                },
                other => Self::Tape {
                    command: other.into(),
                },
            },
            RemCommand::Archive { command } => command.into_command(),
            RemCommand::Restore(args) => Self::Archive {
                command: Box::new(ArchiveCommand::Extract(args.into())),
            },
            RemCommand::Dev { command } => Self::Dev { command },
        }
    }
}

/// Return the library serial a state-changing subcommand targets,
/// or `None` for read-only subcommands. Used by [`run`] to gate
/// the `--allow` check *before* discovery probes the host — an
/// operator who forgot `--allow` should see the allowlist error
/// immediately, not after the CLI talks to `/dev/sg*`.
fn state_changing_target(cmd: &Command) -> Option<&str> {
    match cmd {
        Command::Move { serial, .. }
        | Command::Load { serial, .. }
        | Command::Unload { serial, .. }
        | Command::Export { serial, .. }
        | Command::Import { serial, .. }
        | Command::Rescan { serial }
        | Command::Lock { serial }
        | Command::Unlock { serial } => Some(serial.as_str()),
        Command::Archive { command } => command.tape_target(),
        Command::Dev { command } => Some(command.tape_target()),
        Command::Libraries { .. }
        | Command::Library { .. }
        | Command::Watch { .. }
        | Command::RebuildCatalogFromJournals { .. }
        | Command::Catalog { .. }
        | Command::DaemonClient { .. }
        | Command::OperationClient { .. }
        | Command::CatalogClient { .. }
        | Command::AuditClient { .. }
        | Command::DriveClient { .. }
        | Command::AlarmsClient { .. }
        | Command::Top { .. }
        | Command::TapeAlertsAlias { .. }
        | Command::ArchiveVerifyClient { .. }
        | Command::Tape { .. } => None,
    }
}

/// Parse an SMC element address from a CLI argument. Accepts
/// `0x0400`, `0X0400`, or `1024` — anything that fits in a `u16`.
fn parse_element_addr(s: &str) -> Result<u16, String> {
    let trimmed = s.trim();
    if let Some(rest) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u16::from_str_radix(rest, 16).map_err(|e| format!("invalid hex element address {s:?}: {e}"))
    } else {
        trimmed
            .parse::<u16>()
            .map_err(|e| format!("invalid element address {s:?}: {e}"))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TapeInitTarget {
    Voltag(String),
    Element(u16),
    SlotRange { start: u16, end: u16 },
}

impl TapeInitTarget {
    fn is_batch(&self) -> bool {
        matches!(self, Self::SlotRange { start, end } if start != end)
    }
}

fn parse_tape_init_target(s: &str) -> Result<TapeInitTarget, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("tape init target cannot be empty".to_string());
    }
    for separator in ["..", "-"] {
        if let Some((start, end)) = trimmed.split_once(separator) {
            let start = parse_element_addr(start)?;
            let end = parse_element_addr(end)?;
            if start > end {
                return Err(format!(
                    "slot range start 0x{start:04x} is greater than end 0x{end:04x}"
                ));
            }
            return Ok(TapeInitTarget::SlotRange { start, end });
        }
    }
    match parse_element_addr(trimmed) {
        Ok(element) => Ok(TapeInitTarget::Element(element)),
        Err(_) => Ok(TapeInitTarget::Voltag(trimmed.to_string())),
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

/// Parse an RAO archive build chunk size.
fn parse_archive_chunk_size(s: &str) -> Result<usize, String> {
    let bytes = parse_binary_byte_count(s, "chunk size")?;
    if bytes == 0 {
        return Err("chunk size must be greater than zero".to_string());
    }
    if bytes % 512 != 0 {
        return Err(format!("chunk size {bytes} must be a multiple of 512"));
    }
    usize::try_from(bytes).map_err(|_| format!("chunk size {s:?} is too large for this host"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArchiveByteRange {
    start: u64,
    len: u64,
}

/// Parse an archive member byte range as `start:length`.
fn parse_archive_byte_range(s: &str) -> Result<ArchiveByteRange, String> {
    let (start, len) = s
        .split_once(':')
        .ok_or_else(|| "archive range must be formatted as start:length".to_string())?;
    let start = parse_binary_byte_count(start, "range start")?;
    let len = parse_binary_byte_count(len, "range length")?;
    Ok(ArchiveByteRange { start, len })
}

fn parse_archive_file_size_bytes(s: &str) -> Result<u64, String> {
    parse_binary_byte_count(s, "file size")
}

/// Parse a tape-init block size override and apply the same limits as pool config.
fn parse_tape_block_size(s: &str) -> Result<u64, String> {
    let bytes = parse_binary_byte_count(s, "block size")?;
    remanence_state::validate_block_size(bytes)
        .map_err(|error| format!("invalid block size {s:?}: {error}"))?;
    Ok(bytes)
}

fn parse_binary_byte_count(s: &str, label: &str) -> Result<u64, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(format!("{label} cannot be empty"));
    }
    let lower = trimmed.to_ascii_lowercase();
    let (number, multiplier) = if let Some(number) = lower.strip_suffix("mib") {
        (number, 1024_u64 * 1024)
    } else if let Some(number) = lower.strip_suffix("mb") {
        (number, 1024_u64 * 1024)
    } else if let Some(number) = lower.strip_suffix('m') {
        (number, 1024_u64 * 1024)
    } else if let Some(number) = lower.strip_suffix("kib") {
        (number, 1024_u64)
    } else if let Some(number) = lower.strip_suffix("kb") {
        (number, 1024_u64)
    } else if let Some(number) = lower.strip_suffix('k') {
        (number, 1024_u64)
    } else {
        (lower.as_str(), 1_u64)
    };
    let units = number
        .trim()
        .parse::<u64>()
        .map_err(|err| format!("invalid {label} {s:?}: {err}"))?;
    units
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{label} {s:?} overflows u64"))
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List every library `discover()` found on this host.
    #[command(alias = "libs")]
    Libraries {
        /// Emit machine-readable JSON for bringup/orchestrator scripts.
        #[arg(long)]
        json: bool,
    },

    /// Focused view of a single library, by serial.
    #[command(alias = "lib")]
    Library {
        /// Library serial (VPD 0x80 of the medium changer). Run
        /// `rem libraries` to see the available serials.
        serial: String,
        /// Also list every storage slot's status (full/empty + voltag).
        #[arg(long)]
        slots: bool,
        /// Emit machine-readable JSON with drives, slots, and loaded barcodes.
        #[arg(long)]
        json: bool,
    },

    /// Move a cartridge between two element addresses.
    Move {
        /// Library serial.
        serial: String,
        /// Source element address (slot, IE port, or drive bay).
        #[arg(long, value_parser = parse_element_addr)]
        src: u16,
        /// Destination element address.
        #[arg(long, value_parser = parse_element_addr)]
        dst: u16,
    },

    /// Load a cartridge from a slot into a drive bay
    /// (composed: changer MOVE MEDIUM + SSC LOAD).
    Load {
        /// Library serial.
        serial: String,
        /// Source storage slot.
        #[arg(long, value_parser = parse_element_addr)]
        slot: u16,
        /// Destination drive bay.
        #[arg(long, value_parser = parse_element_addr)]
        bay: u16,
    },

    /// Unload a cartridge from a drive bay
    /// (composed: SSC UNLOAD + changer MOVE MEDIUM).
    Unload {
        /// Library serial.
        serial: String,
        /// Drive bay to unload.
        #[arg(long, value_parser = parse_element_addr)]
        bay: u16,
        /// Destination slot. Defaults to the bay's recorded
        /// `source_slot` (where the cartridge originally came
        /// from) — pass this flag to override.
        #[arg(long, value_parser = parse_element_addr)]
        dest: Option<u16>,
    },

    /// Export a cartridge from a slot to the first available IE port.
    Export {
        /// Library serial.
        serial: String,
        /// Source slot.
        #[arg(long, value_parser = parse_element_addr)]
        slot: u16,
    },

    /// Import a cartridge from the first occupied IE port to a slot.
    Import {
        /// Library serial.
        serial: String,
        /// Destination slot.
        #[arg(long, value_parser = parse_element_addr)]
        slot: u16,
    },

    /// INITIALIZE ELEMENT STATUS on the changer, then reconcile.
    /// Re-derives the changer's internal element state and rebuilds
    /// the snapshot. Shape mismatch is a hard error.
    Rescan {
        /// Library serial.
        serial: String,
    },

    /// PREVENT MEDIUM REMOVAL on the changer. The lock is held by
    /// the changer itself — *not* tied to this CLI process — so it
    /// persists after `rem-debug lock` returns. Released by:
    ///
    /// - `rem-debug unlock <serial>`, the intended counterpart;
    /// - a device reset (e.g., the next driver re-init), which
    ///   most firmwares treat as an implicit ALLOW; or
    /// - a power-cycle of the changer.
    ///
    /// While the lock is held, the front-panel eject button and
    /// operator-initiated mailslot eject are refused by firmware.
    /// Host-initiated SCSI commands are unaffected.
    Lock {
        /// Library serial.
        serial: String,
    },

    /// ALLOW MEDIUM REMOVAL on the changer. Releases a lock set by
    /// `rem-debug lock` or by a daemon that died mid-critical-section.
    Unlock {
        /// Library serial.
        serial: String,
    },

    /// Subscribe to OS hot-plug events touching SCSI tape / changer
    /// subsystems and pretty-print coalesced bursts.
    ///
    /// Debugging tool, not a daemon: runs in the foreground, prints
    /// one line per coalesced burst, exits when the source terminates
    /// or on Ctrl-C. Requires the `linux-udev` Cargo feature at
    /// build time (`cargo build --features linux-udev`), which in
    /// turn requires `pkg-config` and `libudev-dev` system packages.
    ///
    /// Read-only with respect to SCSI: no CDB is ever issued. The
    /// `--allow` flag is ignored.
    Watch {
        /// Sliding coalescing window. Events arriving within this
        /// duration of the previous event collapse into one burst.
        /// Accepts e.g. `500ms`, `1s`, `0` (disabled — each raw
        /// event becomes its own burst).
        #[arg(long, value_name = "DURATION", default_value = "500ms")]
        coalesce_window: String,
    },

    /// Rebuild the local SQLite catalog projection from audit and tape journals.
    RebuildCatalogFromJournals {
        /// Path to `/etc/rem/config.toml`.
        #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
        config: PathBuf,
    },

    /// Local catalog maintenance commands.
    Catalog {
        /// Catalog command to run.
        #[command(subcommand)]
        command: LocalCatalogCommand,
    },

    /// Query daemon process health and version over Layer 5 gRPC.
    #[command(name = "daemon", hide = true)]
    DaemonClient {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Daemon command to run.
        #[command(subcommand)]
        command: DaemonClientCommand,
    },

    /// Query daemon operation status.
    #[command(name = "op", alias = "operation", hide = true)]
    OperationClient {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Operation command to run.
        #[command(subcommand)]
        command: OperationClientCommand,
    },

    /// Query the daemon catalog.
    #[command(name = "daemon-catalog", hide = true)]
    CatalogClient {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Catalog command to run.
        #[command(subcommand)]
        command: CatalogClientCommand,
    },

    /// Query the daemon audit log.
    #[command(name = "audit", hide = true)]
    AuditClient {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit one JSON object per audit entry.
        #[arg(long, global = true)]
        json: bool,
        /// Audit command to run.
        #[command(subcommand)]
        command: AuditClientCommand,
    },

    /// Query and mutate daemon drive stewardship state.
    #[command(name = "drive", hide = true)]
    DriveClient {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Drive command to run.
        #[command(subcommand)]
        command: DriveClientCommand,
    },

    /// List or acknowledge standing daemon alarms.
    #[command(name = "alarms", hide = true)]
    AlarmsClient {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        #[arg(long, global = true)]
        json: bool,
        /// Include cleared alarms.
        #[arg(long = "all")]
        all: bool,
        /// Alarm command to run. Omitted means list alarms.
        #[command(subcommand)]
        command: Option<AlarmsClientCommand>,
    },

    /// Live top view over daemon state.
    #[command(hide = true)]
    Top {
        /// Daemon gRPC endpoint URI.
        #[arg(
            long,
            value_name = "URI",
            default_value = DEFAULT_DAEMON_ENDPOINT,
            global = true
        )]
        endpoint: String,
        /// Emit stable JSON instead of the TUI.
        #[arg(long, global = true)]
        json: bool,
        /// Fetch one live-status snapshot and exit.
        #[arg(long, global = true)]
        once: bool,
    },

    /// Deprecated alias for `rem drive alerts`.
    #[command(name = "tape-alerts-alias", hide = true)]
    TapeAlertsAlias {
        /// Daemon gRPC endpoint URI.
        endpoint: String,
        /// Emit stable CLI-shaped JSON.
        json: bool,
        /// Drive serial or UUID.
        drive: String,
    },

    /// Initialize tapes after the destructive-safety gauntlet.
    Tape {
        /// Tape operation to run.
        #[command(subcommand)]
        command: TapeCommand,
    },

    /// Probe, catalog, or restore an archive through a format driver.
    Archive {
        /// Archive operation to run.
        #[command(subcommand)]
        command: Box<ArchiveCommand>,
    },

    /// Verify an object through a read session pinned to a chosen drive.
    #[command(name = "archive-verify-client", hide = true)]
    ArchiveVerifyClient {
        endpoint: String,
        library: String,
        drive: u16,
        locator: String,
        expected_sha256: String,
    },

    /// Development-only hardware helpers.
    Dev {
        /// Development operation to run.
        #[command(subcommand)]
        command: DevCommand,
    },
}

#[derive(Subcommand, Debug)]
enum LocalCatalogCommand {
    /// Destructively reset local Remanence catalog state from the configured paths.
    Reset(CatalogResetArgs),
}

#[derive(Args, Debug)]
struct CatalogResetArgs {
    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,

    /// Required confirmation flag.
    #[arg(long = "i-understand-this-erases-the-catalog")]
    i_understand_this_erases_the_catalog: bool,
}

#[derive(Subcommand, Debug)]
enum DaemonClientCommand {
    /// Return daemon health.
    Health,

    /// Return daemon and API version.
    Version,
}

#[derive(Subcommand, Debug)]
enum OperationClientCommand {
    /// Get one operation status by UUID.
    Get {
        /// Operation UUID.
        operation_id: String,
    },

    /// List known operations.
    List,
}

#[derive(Subcommand, Debug)]
enum CatalogClientCommand {
    /// List cataloged tapes.
    Tapes {
        /// Restrict to one tape pool.
        #[arg(long, value_name = "POOL")]
        pool: Option<String>,
    },

    /// Get one cataloged tape by UUID.
    Tape {
        /// Tape UUID.
        tape_uuid: String,
    },

    /// List tape files for one tape UUID.
    TapeFiles {
        /// Tape UUID.
        tape_uuid: String,
    },

    /// List tape pools.
    Pools,

    /// Get one tape pool by id.
    Pool {
        /// Tape pool id.
        pool_id: String,
    },

    /// Enumerate native and foreign catalog units.
    Units {
        /// Filter by catalog-unit origin.
        #[arg(long, value_enum, default_value_t = CatalogUnitOriginFilterArg::All)]
        origin: CatalogUnitOriginFilterArg,
    },

    /// Get one catalog unit by id.
    Unit {
        /// Catalog unit id.
        unit_id: String,
    },

    /// List entries inside one catalog unit.
    Entries {
        /// Catalog unit id.
        unit_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum DriveClientCommand {
    /// Load a selected tape into a selected drive bay and wait for completion.
    Load(DriveLoadArgs),

    /// List cataloged drives.
    List {
        /// Include foreign drives.
        #[arg(long)]
        foreign: bool,
        /// Include retired drives.
        #[arg(long)]
        retired: bool,
    },

    /// Show one drive by serial or UUID.
    Show {
        /// Drive serial or UUID.
        drive: String,
    },

    /// Show one drive's history.
    History {
        /// Drive serial or UUID.
        drive: String,
        /// Include observational events.
        #[arg(long)]
        events: bool,
        /// Include health snapshots.
        #[arg(long)]
        snapshots: bool,
    },

    /// Show one drive's active alerts. `rem tape alerts` is a deprecated alias.
    Alerts {
        /// Drive serial or UUID.
        drive: String,
    },

    /// Annotate one drive.
    Annotate(DriveAnnotateArgs),

    /// Permanently remove one drive from the managed fleet.
    Retire(DriveRetireArgs),

    /// Poll one drive now.
    Poll {
        /// Drive serial or UUID.
        drive: String,
    },

    /// Clean one drive now.
    Clean {
        /// Drive serial or UUID.
        drive: String,
    },
}

#[derive(Args, Debug)]
#[command(group(
    clap::ArgGroup::new("source")
        .required(true)
        .multiple(false)
        .args(["barcode", "slot"])
))]
struct DriveLoadArgs {
    /// Library serial or UUID.
    #[arg(long, value_name = "LIBRARY")]
    library: String,

    /// Tape barcode to find in the current slot inventory.
    #[arg(long, value_name = "BARCODE")]
    barcode: Option<String>,

    /// Source slot element address.
    #[arg(long, value_parser = parse_element_addr)]
    slot: Option<u16>,

    /// Destination drive bay element address.
    #[arg(long, value_parser = parse_element_addr)]
    bay: u16,

    /// Return after legacy LOAD completion without waiting for cartridge readiness.
    #[arg(long)]
    no_wait: bool,
}

#[derive(Subcommand, Debug)]
enum AuditClientCommand {
    /// Stream entries in the half-open interval [since, until).
    Query {
        /// Inclusive RFC3339 lower bound.
        #[arg(long, value_name = "RFC3339")]
        since: String,
        /// Exclusive RFC3339 upper bound.
        #[arg(long, value_name = "RFC3339")]
        until: String,
        /// Exact-match filter in k=v form; repeat for multiple fields.
        #[arg(long = "filter", value_name = "K=V")]
        filters: Vec<String>,
    },
}

#[derive(Args, Debug)]
struct DriveAnnotateArgs {
    /// Drive serial or UUID.
    drive: String,
    /// Purchase date, YYYY-MM-DD.
    #[arg(long)]
    purchase_date: Option<String>,
    /// Warranty end date, YYYY-MM-DD.
    #[arg(long)]
    warranty_until: Option<String>,
    /// Display-only cost string.
    #[arg(long)]
    cost: Option<String>,
    /// Append a timestamped note.
    #[arg(long)]
    note: Option<String>,
    /// Replace notes.
    #[arg(long)]
    notes_set: Option<String>,
    /// Permit mutation of a Derived-identity row.
    #[arg(long)]
    allow_derived_identity: bool,
}

#[derive(Args, Debug)]
struct DriveRetireArgs {
    /// Drive serial or UUID.
    drive: String,
    /// Operator-supplied reason.
    #[arg(long)]
    reason: String,
    /// Required server-side acknowledgement.
    #[arg(long = "i-understand-fleet-removal-is-permanent")]
    i_understand_fleet_removal_is_permanent: bool,
    /// Permit mutation of a Derived-identity row.
    #[arg(long)]
    allow_derived_identity: bool,
}

#[derive(Subcommand, Debug)]
enum AlarmsClientCommand {
    /// Acknowledge one alarm condition key.
    Ack {
        /// Alarm condition key.
        condition_key: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum CatalogUnitOriginFilterArg {
    All,
    Native,
    Foreign,
}

#[derive(Subcommand, Debug)]
enum RemTapeCommand {
    /// Deprecated alias; use `rem drive alerts <serial|uuid>`.
    Alerts(TapeAlertsAliasArgs),

    /// Initialize one tape or a slot range.
    Init(TapeInitArgs),

    /// Poll TEST UNIT READY until already-loaded media is usable.
    WaitReady(TapeWaitReadyArgs),

    /// Inspect or release media-readiness quarantine fences.
    Quarantine {
        /// Quarantine operation.
        #[command(subcommand)]
        command: TapeQuarantineCommand,
    },

    /// Permanently retire one tape identity in the local catalog.
    Retire(TapeRetireArgs),
}

impl From<RemTapeCommand> for TapeCommand {
    fn from(value: RemTapeCommand) -> Self {
        match value {
            RemTapeCommand::Alerts(_) => {
                unreachable!("rem tape alerts is dispatched as a daemon drive-alerts alias")
            }
            RemTapeCommand::Init(args) => Self::Init(args),
            RemTapeCommand::WaitReady(args) => Self::WaitReady(args),
            RemTapeCommand::Quarantine { command } => Self::Quarantine { command },
            RemTapeCommand::Retire(args) => Self::Retire(args),
        }
    }
}

#[derive(Subcommand, Debug)]
enum TapeCommand {
    /// Read the loaded drive's TapeAlert LOG SENSE page.
    Alerts(TapeAlertsArgs),

    /// Initialize one tape or a slot range.
    Init(TapeInitArgs),

    /// Poll TEST UNIT READY until already-loaded media is usable.
    WaitReady(TapeWaitReadyArgs),

    /// Inspect or release media-readiness quarantine fences.
    Quarantine {
        /// Quarantine operation.
        #[command(subcommand)]
        command: TapeQuarantineCommand,
    },

    /// Permanently retire one tape identity in the local catalog.
    Retire(TapeRetireArgs),
}

impl TapeCommand {
    fn validate_before_discovery(&self) -> Result<(), String> {
        match self {
            Self::Alerts(args) => args.validate_before_discovery(),
            Self::Init(args) => args.validate_before_discovery(),
            Self::WaitReady(args) => args.validate_before_discovery(),
            Self::Quarantine { command } => command.validate_before_discovery(),
            Self::Retire(args) => args.validate_before_discovery(),
        }
    }
}

#[derive(Args, Debug)]
struct TapeAlertsArgs {
    /// Drive bay element address to query.
    #[arg(long, value_parser = parse_element_addr)]
    bay: u16,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,

    /// Select a configured library when more than one is present.
    #[arg(long, value_name = "SERIAL")]
    library: Option<String>,
}

#[derive(Args, Debug)]
struct TapeAlertsAliasArgs {
    /// Drive serial or UUID. Deprecated: use `rem drive alerts <serial|uuid>`.
    drive: String,

    /// Daemon gRPC endpoint URI.
    #[arg(long, value_name = "URI", default_value = DEFAULT_DAEMON_ENDPOINT)]
    endpoint: String,

    /// Emit stable CLI-shaped JSON.
    #[arg(long)]
    json: bool,
}

impl TapeAlertsArgs {
    fn validate_before_discovery(&self) -> Result<(), String> {
        if self
            .library
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("tape alerts --library cannot be empty".to_string());
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
struct TapeInitArgs {
    /// Barcode, element address, or inclusive slot range such as 0x0400..0x0407.
    target: String,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,

    /// Select a library when the target is not globally unique.
    #[arg(long, value_name = "SERIAL")]
    library: Option<String>,

    /// Run every check but write nothing.
    #[arg(long)]
    dry_run: bool,

    /// Scoped override for `RequireForce` decisions only.
    #[arg(long)]
    force: bool,

    /// Dangerous data-clobber override. Rejected for dry-run and batch init.
    #[arg(long)]
    clobber_data: bool,

    /// Fixed tape block size for a fresh initialization.
    #[arg(long, value_name = "BYTES", value_parser = parse_tape_block_size)]
    block_size: Option<u64>,
}

impl TapeInitArgs {
    fn validate_before_discovery(&self) -> Result<(), String> {
        let target = parse_tape_init_target(self.target.as_str())?;
        if self.clobber_data && self.dry_run {
            return Err("tape init rejects --clobber-data with --dry-run".to_string());
        }
        if self.clobber_data && target.is_batch() {
            return Err("tape init rejects --clobber-data for batch slot ranges".to_string());
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
struct TapeWaitReadyArgs {
    /// Resume a durable media-readiness operation without moving media.
    #[arg(long, value_name = "UUID", conflicts_with_all = ["barcode", "drive_element"])]
    resume: Option<Uuid>,

    /// Barcode to find in an already-loaded drive.
    #[arg(long, value_name = "BARCODE", conflicts_with = "drive_element")]
    barcode: Option<String>,

    /// Drive bay element address to poll.
    #[arg(long, value_parser = parse_element_addr, conflicts_with = "barcode")]
    drive_element: Option<u16>,

    /// Acknowledge that the target cartridge is already in the drive.
    #[arg(long)]
    already_loaded: bool,

    /// Keep polling retryable readiness states until timeout.
    #[arg(long)]
    wait: bool,

    /// Maximum wait duration. Accepts ms/s/m/h suffixes.
    #[arg(long, value_parser = parse_wait_duration_arg, default_value = "2.5h")]
    timeout: StdDuration,

    /// Poll interval. Accepts ms/s/m/h suffixes.
    #[arg(long, value_parser = parse_wait_duration_arg, default_value = "30s")]
    poll: StdDuration,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,

    /// Select a configured library.
    #[arg(long, value_name = "SERIAL")]
    library: Option<String>,

    /// Emit stable CLI-shaped JSON.
    #[arg(long)]
    json: bool,
}

impl TapeWaitReadyArgs {
    fn validate_before_discovery(&self) -> Result<(), String> {
        if self.resume.is_none() && self.barcode.is_none() && self.drive_element.is_none() {
            return Err(
                "tape wait-ready requires --resume, --barcode, or --drive-element".to_string(),
            );
        }
        if self.resume.is_some() && self.already_loaded {
            return Err("tape wait-ready --resume already implies loaded media".to_string());
        }
        if self.drive_element.is_some() && !self.already_loaded {
            return Err("tape wait-ready --drive-element requires --already-loaded".to_string());
        }
        if self
            .barcode
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("tape wait-ready --barcode cannot be empty".to_string());
        }
        if self
            .library
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("tape wait-ready --library cannot be empty".to_string());
        }
        if self.wait && self.timeout.is_zero() {
            return Err(
                "tape wait-ready --timeout must be greater than zero with --wait".to_string(),
            );
        }
        if self.wait && self.poll.is_zero() {
            return Err("tape wait-ready --poll must be greater than zero with --wait".to_string());
        }
        Ok(())
    }
}

fn parse_wait_duration_arg(s: &str) -> Result<StdDuration, String> {
    let trimmed = s.trim();
    if trimmed == "0" {
        return Ok(StdDuration::ZERO);
    }
    let (number, multiplier) = if let Some(value) = trimmed.strip_suffix("ms") {
        (value.trim(), 0.001_f64)
    } else if let Some(value) = trimmed.strip_suffix('s') {
        (value.trim(), 1.0_f64)
    } else if let Some(value) = trimmed.strip_suffix('m') {
        (value.trim(), 60.0_f64)
    } else if let Some(value) = trimmed.strip_suffix('h') {
        (value.trim(), 3600.0_f64)
    } else {
        return Err(format!(
            "invalid duration {s:?}: expected '<number>ms', '<number>s', '<number>m', '<number>h', or '0'"
        ));
    };
    let parsed: f64 = number
        .parse()
        .map_err(|error| format!("invalid duration {s:?}: {error}"))?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(format!(
            "invalid duration {s:?}: must be finite and non-negative"
        ));
    }
    Ok(StdDuration::from_secs_f64(parsed * multiplier))
}

#[derive(Subcommand, Debug)]
enum TapeQuarantineCommand {
    /// List active media-readiness fences.
    List {
        /// Path to `/etc/rem/config.toml`.
        #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
        config: PathBuf,

        /// Restrict to one selected library serial.
        #[arg(long, value_name = "SERIAL")]
        library: Option<String>,

        /// Emit stable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show one active media-readiness fence.
    Show {
        /// Quarantine id or media-readiness operation UUID.
        quarantine: String,

        /// Path to `/etc/rem/config.toml`.
        #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
        config: PathBuf,

        /// Emit stable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Release one media-readiness fence after operator/RCA acknowledgement.
    Release {
        /// Quarantine id or media-readiness operation UUID.
        quarantine: String,

        /// Path to `/etc/rem/config.toml`.
        #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
        config: PathBuf,

        /// Confirm that inventory has settled after the unknown operation.
        #[arg(long)]
        after_settled_inventory: bool,

        /// Operator/RCA acknowledgement text.
        #[arg(long, value_name = "TEXT")]
        ack: String,

        /// Emit stable JSON.
        #[arg(long)]
        json: bool,
    },
}

impl TapeQuarantineCommand {
    fn validate_before_discovery(&self) -> Result<(), String> {
        match self {
            Self::List { library, .. } => {
                if library
                    .as_deref()
                    .is_some_and(|value| value.trim().is_empty())
                {
                    return Err("tape quarantine list --library cannot be empty".to_string());
                }
            }
            Self::Show { quarantine, .. } => {
                if quarantine.trim().is_empty() {
                    return Err("tape quarantine show requires an id".to_string());
                }
            }
            Self::Release {
                quarantine,
                after_settled_inventory,
                ack,
                ..
            } => {
                if quarantine.trim().is_empty() {
                    return Err("tape quarantine release requires an id".to_string());
                }
                if !after_settled_inventory {
                    return Err(
                        "tape quarantine release requires --after-settled-inventory".to_string()
                    );
                }
                if ack.trim().is_empty() {
                    return Err("tape quarantine release --ack cannot be empty".to_string());
                }
            }
        }
        Ok(())
    }
}

#[derive(Args, Debug)]
#[command(
    long_about = "Permanently retire one tape identity in the local catalog.

Retiring ends an identity's life: the row becomes terminal (`retired`),
its barcode is released for a fresh `rem tape init`, and every committed
object copy on it is marked `missing` — the contents are declared
permanently unreadable. The identity's journals and audit history stay
attached to its uuid forever; a retired row is never reused, even with
force.

This command touches only the local catalog and audit log — no SCSI, no
library allowlist. It takes the exclusive state lock, so it fails cleanly
while the daemon is running: stop the daemon first (the recycle scripts
already do)."
)]
struct TapeRetireArgs {
    /// Barcode (voltag) or 32-hex tape UUID of the identity to retire.
    target: String,

    /// Why this identity is being retired, such as `recycled` or
    /// `vtl-rebuilt` (free text, recorded in the audit log).
    #[arg(long, value_name = "TEXT")]
    reason: String,

    /// Required acknowledgement that every copy on this tape becomes
    /// permanently unreadable.
    #[arg(long = "i-understand-copies-become-unreadable")]
    copies_unreadable_ack: bool,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,

    /// Report what would be retired without writing.
    #[arg(long)]
    dry_run: bool,

    /// Emit stable CLI-shaped JSON (`rem.tape.retire.v1`).
    #[arg(long)]
    json: bool,
}

impl TapeRetireArgs {
    fn validate_before_discovery(&self) -> Result<(), String> {
        if self.target.trim().is_empty() {
            return Err("tape retire target cannot be empty".to_string());
        }
        if !self.copies_unreadable_ack {
            return Err("tape retire requires --i-understand-copies-become-unreadable".to_string());
        }
        Ok(())
    }
}

#[derive(Subcommand, Debug)]
enum DevCommand {
    /// Destructively write a BRU dump file to the loaded tape in a drive bay.
    WriteDumpToTape {
        /// Path to a BRU dump file.
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

#[derive(Subcommand, Debug)]
enum RemArchiveCommand {
    /// Print machine-readable archive feature capabilities.
    Capabilities,

    /// Re-seal one recipient envelope to a new recipient set.
    Reseal(ArchiveResealArgs),

    /// Build a portable RAO object file from local inputs.
    Build(RemArchiveBuildArgs),

    /// Inspect a portable RAO object file.
    Inspect(RemArchiveInspectArgs),

    /// Extract a portable RAO object file into a directory.
    Extract(RemArchiveExtractArgs),

    /// Decrypt an encrypted RAO object from stdin to stdout.
    #[command(name = "extract-stream")]
    ExtractStream(RemArchiveExtractStreamArgs),

    /// Query the stored ciphertext frames covering a plaintext member range.
    #[command(name = "covering-range")]
    CoveringRange(RemArchiveCoveringRangeArgs),

    /// Probe an archive dump without streaming entries.
    Probe(RemArchiveInputArgs),

    /// Catalog entries from an archive dump.
    Scan(RemArchiveInputArgs),

    /// Restore files from an archive dump into a directory.
    Restore(RemArchiveRestoreArgs),

    /// Recover files from an archive dump into sparse partial files.
    Recover(RemArchiveRecoverArgs),

    /// Compatibility parser for direct local archive writes.
    #[command(hide = true)]
    Write(RemArchiveWriteArgs),

    /// Compatibility parser for direct local archive reads.
    #[command(hide = true)]
    Read(RemArchiveReadArgs),

    /// Compatibility parser for direct local stored-object export.
    #[command(name = "export-object", hide = true)]
    ExportObject(RemArchiveExportObjectArgs),

    /// Verify an object through the tape currently loaded in a chosen drive.
    Verify(RemArchiveVerifyArgs),

    /// List native objects from the local catalog (no tape access).
    List(RemArchiveListArgs),
}

/// Arguments for `rem archive build`.
#[derive(Args, Debug)]
struct RemArchiveBuildArgs {
    /// Input files or directories. Directories are expanded recursively.
    #[arg(
        long = "inputs",
        value_name = "PATH",
        num_args = 1..,
        required_unless_present_any = ["scan_only", "map"]
    )]
    inputs: Vec<PathBuf>,

    /// Source-map TSV emitted by a trusted upstream planner.
    #[arg(
        long = "map",
        value_name = "PATH",
        conflicts_with_all = ["inputs", "rules", "scan_only"],
        requires = "source_root"
    )]
    map: Option<PathBuf>,

    /// Canonical source-root anchor for --map source paths.
    #[arg(long = "source-root", value_name = "DIR")]
    source_root: Option<PathBuf>,

    /// Expected SHA-256 of the source-map TSV.
    #[arg(long = "map-sha256", value_name = "HEX", requires = "map")]
    map_sha256: Option<String>,

    /// Output RAO object file.
    #[arg(
        long,
        value_name = "PATH",
        required_unless_present = "scan_only",
        conflicts_with = "scan_only"
    )]
    out: Option<PathBuf>,

    /// Ordered ingest ruleset for blob/exclude policy.
    #[arg(long, value_name = "PATH")]
    rules: Option<PathBuf>,

    /// Classify inputs and emit the clustered ingest report without writing RAO.
    #[arg(long)]
    scan_only: bool,

    /// Write the customer-facing member manifest JSON.
    #[arg(long, value_name = "PATH")]
    manifest_out: Option<PathBuf>,

    /// Disable generated `.remwrap.idx` siblings for blob wrappers.
    #[arg(long)]
    no_index: bool,

    /// Dense-subtree ratio threshold for blob suggestions.
    #[arg(long, value_name = "RATIO", default_value = "0.9")]
    blob_suggest_ratio: f64,

    /// Minimum non-compliant entries before suggesting a blob.
    #[arg(long, value_name = "COUNT", default_value = "100")]
    blob_suggest_count: u64,

    /// Non-compliant count above which scan reports a sanity-ceiling verdict.
    #[arg(long, value_name = "COUNT", default_value = "10000")]
    sanity_ceiling_count: u64,

    /// Canonical RAOR recipient public-key file; repeat 2 to 8 times.
    #[arg(long = "recipient", value_name = "RAOR")]
    recipients: Vec<PathBuf>,

    /// Object id to record in the RAO global header (default: fresh UUID).
    #[arg(long, value_name = "ID")]
    object_id: Option<String>,

    /// Opaque caller / orchestrator object id (default: object id).
    #[arg(long, value_name = "ID")]
    caller_object_id: Option<String>,

    /// Manifest file id (default: fresh UUID).
    #[arg(long, value_name = "ID")]
    manifest_file_id: Option<String>,

    /// RFC3339 write timestamp (default: current UTC time).
    #[arg(long, value_name = "RFC3339")]
    timestamp: Option<String>,

    /// RAO object block/chunk size.
    #[arg(long, value_name = "BYTES", value_parser = parse_archive_chunk_size, default_value = "256KiB")]
    chunk_size: usize,
}

/// Arguments for `rem archive inspect`.
#[derive(Args, Debug)]
struct RemArchiveInspectArgs {
    /// Input RAO object file.
    #[arg(long, value_name = "PATH")]
    object: PathBuf,

    /// RAO object block/chunk size for plaintext objects.
    #[arg(long, value_name = "BYTES", value_parser = parse_archive_chunk_size, default_value = "256KiB")]
    chunk_size: usize,
}

/// Arguments for `rem archive extract`.
#[derive(Args, Debug)]
struct RemArchiveExtractArgs {
    /// Input RAO object file.
    #[arg(long, value_name = "PATH")]
    object: PathBuf,

    /// Destination directory.
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,

    /// RAO object block/chunk size for plaintext objects.
    #[arg(long, value_name = "BYTES", value_parser = parse_archive_chunk_size, default_value = "256KiB")]
    chunk_size: usize,

    /// Canonical RAOP private-key file. Required for encrypted objects.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Archive member path for byte-range extraction.
    #[arg(long = "path", value_name = "RAO_PATH")]
    path: Option<String>,

    /// First object-local BodyLba for --path, from the build report or catalog row.
    #[arg(long = "first-chunk-lba", value_name = "LBA")]
    first_chunk_lba: Option<u64>,

    /// Full member size for --path, from the build report or catalog row.
    #[arg(long = "file-size-bytes", value_name = "BYTES", value_parser = parse_archive_file_size_bytes)]
    file_size_bytes: Option<u64>,

    /// Member byte range to extract, formatted as start:length.
    #[arg(long = "range", value_name = "START:LEN", value_parser = parse_archive_byte_range)]
    range: Option<ArchiveByteRange>,

    /// Replace existing destination files.
    #[arg(long)]
    overwrite: bool,

    /// Keep `.remwrap.tar` and `.remwrap.idx` entries literal instead of unwrapping.
    #[arg(long)]
    no_unwrap: bool,

    /// RAO blob wrapper entry to use for single-member restore.
    #[arg(long = "blob-entry", value_name = "RAO_PATH", requires = "blob_member")]
    blob_entry: Option<String>,

    /// Member path inside --blob-entry to restore.
    #[arg(long = "blob-member", value_name = "TAR_PATH", requires = "blob_entry")]
    blob_member: Option<String>,
}

/// Arguments for `rem archive extract-stream`.
#[derive(Args, Debug)]
struct RemArchiveExtractStreamArgs {
    /// Canonical RAOP private-key file used to decrypt the envelope.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Absolute plaintext byte range to write, formatted as start:length.
    #[arg(long = "range", value_name = "START:LEN", value_parser = parse_archive_byte_range)]
    range: Option<ArchiveByteRange>,

    /// Authenticated header+metadata prefix for ranged-ciphertext input.
    #[arg(
        long,
        value_name = "PATH",
        requires = "stored_range_start",
        requires = "range"
    )]
    authenticated_prefix: Option<PathBuf>,

    /// Absolute stored offset at which ranged ciphertext stdin begins.
    #[arg(
        long,
        value_name = "BYTE",
        requires = "authenticated_prefix",
        requires = "range"
    )]
    stored_range_start: Option<u64>,
}

/// Arguments for `rem archive covering-range`.
#[derive(Args, Debug)]
struct RemArchiveCoveringRangeArgs {
    /// Canonical RAOP private-key file used to authenticate the envelope.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Expected encrypted object identifier.
    #[arg(long, value_name = "ID")]
    object_id: String,

    /// Caller member identifier, echoed in the machine-readable response.
    #[arg(long, value_name = "ID")]
    file_id: String,

    /// Absolute plaintext member range, formatted as start:length.
    #[arg(long = "range", value_name = "START:LEN", value_parser = parse_archive_byte_range)]
    range: ArchiveByteRange,
}

/// Arguments for `rem archive write`.
#[derive(Args, Debug)]
struct RemArchiveWriteArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Local file to write.
    #[arg(long, value_name = "PATH")]
    file: PathBuf,

    /// Tape pool to target (e.g. `scenario-a`).
    #[arg(long, value_name = "POOL")]
    pool: String,

    /// Override the in-archive path (default: file basename).
    #[arg(long, value_name = "PATH")]
    archive_path: Option<PathBuf>,

    /// Opaque caller / orchestrator object id (default: fresh UUID).
    #[arg(long, value_name = "ID")]
    caller_object_id: Option<String>,

    /// Canonical RAOR recipient public-key file; repeat 2 to 8 times.
    #[arg(long = "recipient", value_name = "RAOR")]
    recipients: Vec<PathBuf>,

    /// Emit the locator as one JSON line to stdout (seam contract §4).
    #[arg(long)]
    json: bool,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}

/// Arguments for `rem archive read`.
#[derive(Args, Debug)]
struct RemArchiveReadArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Destination path for the restored payload bytes.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Canonical RAOP private-key file. Required for encrypted copies.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}

/// Arguments for `rem archive export-object`.
#[derive(Args, Debug)]
struct RemArchiveExportObjectArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Destination path for the complete stored object bytes.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}

/// Arguments for `rem archive verify`.
#[derive(Args, Debug)]
struct RemArchiveVerifyArgs {
    /// Daemon gRPC endpoint URI.
    #[arg(long, value_name = "URI", default_value = DEFAULT_DAEMON_ENDPOINT)]
    endpoint: String,

    /// Library serial or UUID.
    #[arg(long, value_name = "LIBRARY")]
    library: String,

    /// Drive bay containing the tape to verify.
    #[arg(long, value_parser = parse_element_addr)]
    drive: u16,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Expected payload SHA-256 (hex) to compare the tape bytes against.
    #[arg(long, value_name = "HEX")]
    expected_sha256: String,
}

/// Arguments for `rem archive list`.
#[derive(Args, Debug)]
struct RemArchiveListArgs {
    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}

#[derive(Args, Debug)]
struct RemArchiveInputArgs {
    /// Archive format driver.
    #[arg(long, value_enum)]
    format: ArchiveFormat,

    #[command(flatten)]
    source: RemArchiveSourceArgs,
}

#[derive(Args, Debug)]
struct RemArchiveRestoreArgs {
    /// Archive format driver.
    #[arg(long, value_enum)]
    format: ArchiveFormat,

    #[command(flatten)]
    source: RemArchiveSourceArgs,

    /// Destination directory.
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,

    /// Replace existing destination files.
    #[arg(long)]
    overwrite: bool,
}

#[derive(Args, Debug)]
struct RemArchiveRecoverArgs {
    /// Archive format driver.
    #[arg(long, value_enum)]
    format: ArchiveFormat,

    #[command(flatten)]
    source: RemArchiveSourceArgs,

    /// Destination directory.
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,
}

#[derive(Args, Debug)]
struct RemArchiveSourceArgs {
    /// Path to a byte-stream archive dump. Required for normal `rem`
    /// archive commands.
    #[arg(long, value_name = "PATH", conflicts_with = "tape")]
    dump: Option<PathBuf>,

    /// Hidden compatibility parser for legacy `rem archive ... --tape`.
    #[arg(
        long,
        value_name = "SERIAL",
        conflicts_with = "dump",
        requires = "bay",
        hide = true
    )]
    tape: Option<String>,

    /// Hidden compatibility parser for legacy `rem archive ... --tape`.
    #[arg(long, value_parser = parse_element_addr, requires = "tape", hide = true)]
    bay: Option<u16>,

    /// Hidden compatibility parser for legacy `rem archive ... --tape`.
    #[arg(long, requires = "tape", hide = true)]
    rewind: bool,
}

impl RemArchiveCommand {
    fn into_command(self) -> Command {
        let archive = match self {
            Self::Verify(args) => {
                return Command::ArchiveVerifyClient {
                    endpoint: args.endpoint,
                    library: args.library,
                    drive: args.drive,
                    locator: args.locator,
                    expected_sha256: args.expected_sha256,
                };
            }
            Self::Capabilities => ArchiveCommand::Capabilities,
            Self::Reseal(args) => ArchiveCommand::Reseal(args),
            Self::Build(args) => ArchiveCommand::Build(args.into()),
            Self::Inspect(args) => ArchiveCommand::Inspect(args.into()),
            Self::Extract(args) => ArchiveCommand::Extract(args.into()),
            Self::ExtractStream(args) => ArchiveCommand::ExtractStream(args.into()),
            Self::CoveringRange(args) => ArchiveCommand::CoveringRange(args.into()),
            Self::Probe(args) => ArchiveCommand::Probe(args.into()),
            Self::Scan(args) => ArchiveCommand::Scan(args.into()),
            Self::Restore(args) => ArchiveCommand::Restore(args.into()),
            Self::Recover(args) => ArchiveCommand::Recover(args.into()),
            Self::Write(args) => ArchiveCommand::Write(args.into()),
            Self::Read(args) => ArchiveCommand::Read(args.into()),
            Self::ExportObject(args) => ArchiveCommand::ExportObject(args.into()),
            Self::List(args) => ArchiveCommand::List(args.into()),
        };
        Command::Archive {
            command: Box::new(archive),
        }
    }
}

impl From<RemArchiveBuildArgs> for ArchiveBuildArgs {
    fn from(value: RemArchiveBuildArgs) -> Self {
        Self {
            inputs: value.inputs,
            map: value.map,
            source_root: value.source_root,
            map_sha256: value.map_sha256,
            out: value.out,
            rules: value.rules,
            scan_only: value.scan_only,
            manifest_out: value.manifest_out,
            no_index: value.no_index,
            blob_suggest_ratio: value.blob_suggest_ratio,
            blob_suggest_count: value.blob_suggest_count,
            sanity_ceiling_count: value.sanity_ceiling_count,
            recipients: value.recipients,
            object_id: value.object_id,
            caller_object_id: value.caller_object_id,
            manifest_file_id: value.manifest_file_id,
            timestamp: value.timestamp,
            chunk_size: value.chunk_size,
        }
    }
}

impl From<RemArchiveInspectArgs> for ArchiveInspectArgs {
    fn from(value: RemArchiveInspectArgs) -> Self {
        Self {
            object: value.object,
            chunk_size: value.chunk_size,
        }
    }
}

impl From<RemArchiveExtractArgs> for ArchiveExtractArgs {
    fn from(value: RemArchiveExtractArgs) -> Self {
        Self {
            object: value.object,
            dest: value.dest,
            chunk_size: value.chunk_size,
            private_key: value.private_key,
            path: value.path,
            first_chunk_lba: value.first_chunk_lba,
            file_size_bytes: value.file_size_bytes,
            range: value.range,
            overwrite: value.overwrite,
            no_unwrap: value.no_unwrap,
            blob_entry: value.blob_entry,
            blob_member: value.blob_member,
        }
    }
}

impl From<RemArchiveExtractStreamArgs> for ArchiveExtractStreamArgs {
    fn from(value: RemArchiveExtractStreamArgs) -> Self {
        Self {
            private_key: value.private_key,
            range: value.range,
            authenticated_prefix: value.authenticated_prefix,
            stored_range_start: value.stored_range_start,
        }
    }
}

impl From<RemArchiveCoveringRangeArgs> for ArchiveCoveringRangeArgs {
    fn from(value: RemArchiveCoveringRangeArgs) -> Self {
        Self {
            private_key: value.private_key,
            object_id: value.object_id,
            file_id: value.file_id,
            range: value.range,
        }
    }
}

impl From<RemArchiveWriteArgs> for ArchiveWriteArgs {
    fn from(value: RemArchiveWriteArgs) -> Self {
        Self {
            library: value.library,
            file: value.file,
            pool_id: value.pool,
            archive_path: value.archive_path,
            caller_object_id: value.caller_object_id,
            recipients: value.recipients,
            json_output: value.json,
            config: value.config,
        }
    }
}

impl From<RemArchiveReadArgs> for ArchiveReadArgs {
    fn from(value: RemArchiveReadArgs) -> Self {
        Self {
            library: value.library,
            locator: value.locator,
            out: value.out,
            private_key: value.private_key,
            config: value.config,
        }
    }
}

impl From<RemArchiveExportObjectArgs> for ArchiveExportObjectArgs {
    fn from(value: RemArchiveExportObjectArgs) -> Self {
        Self {
            library: value.library,
            locator: value.locator,
            out: value.out,
            config: value.config,
        }
    }
}

impl From<RemArchiveListArgs> for ArchiveListArgs {
    fn from(value: RemArchiveListArgs) -> Self {
        Self {
            config: value.config,
        }
    }
}

impl From<RemArchiveInputArgs> for ArchiveInputArgs {
    fn from(value: RemArchiveInputArgs) -> Self {
        Self {
            format: value.format,
            source: value.source.into(),
        }
    }
}

impl From<RemArchiveRestoreArgs> for ArchiveRestoreArgs {
    fn from(value: RemArchiveRestoreArgs) -> Self {
        Self {
            format: value.format,
            source: value.source.into(),
            dest: value.dest,
            overwrite: value.overwrite,
        }
    }
}

impl From<RemArchiveRecoverArgs> for ArchiveRecoverArgs {
    fn from(value: RemArchiveRecoverArgs) -> Self {
        Self {
            format: value.format,
            source: value.source.into(),
            dest: value.dest,
        }
    }
}

impl From<RemArchiveSourceArgs> for ArchiveSourceArgs {
    fn from(value: RemArchiveSourceArgs) -> Self {
        Self {
            dump: value.dump,
            tape: value.tape,
            bay: value.bay,
            rewind: value.rewind,
        }
    }
}

#[derive(Subcommand, Debug)]
enum ArchiveCommand {
    /// Print machine-readable archive feature capabilities.
    Capabilities,

    /// Fully re-seal one recipient envelope to a new recipient set.
    Reseal(ArchiveResealArgs),

    /// Build a portable RAO object file from local inputs.
    Build(ArchiveBuildArgs),

    /// Inspect a portable RAO object file.
    Inspect(ArchiveInspectArgs),

    /// Extract a portable RAO object file into a directory.
    Extract(ArchiveExtractArgs),

    /// Decrypt an encrypted RAO object from stdin to stdout.
    #[command(name = "extract-stream")]
    ExtractStream(ArchiveExtractStreamArgs),

    /// Query the stored ciphertext frames covering a plaintext member range.
    #[command(name = "covering-range")]
    CoveringRange(ArchiveCoveringRangeArgs),

    /// Probe an archive source without streaming entries.
    Probe(ArchiveInputArgs),

    /// Catalog entries from an archive source.
    Scan(ArchiveInputArgs),

    /// Restore files from an archive source into a directory.
    Restore(ArchiveRestoreArgs),

    /// Recover files from an archive source into sparse partial files.
    Recover(ArchiveRecoverArgs),

    /// Write one local file to a pool-selected tape and emit a locator.
    Write(ArchiveWriteArgs),

    /// Read one object back from tape by locator and write it to --out.
    Read(ArchiveReadArgs),

    /// Export one complete stored object from tape by locator.
    #[command(name = "export-object")]
    ExportObject(ArchiveExportObjectArgs),

    /// Verify one object on tape by streaming + hashing, no restore to disk.
    Verify(ArchiveVerifyArgs),

    /// List native objects from the local catalog (no tape access).
    List(ArchiveListArgs),
}

/// Arguments for `rem archive reseal`.
#[derive(Args, Debug)]
struct ArchiveResealArgs {
    /// Existing complete encrypted RAO object.
    #[arg(long, value_name = "PATH")]
    object: PathBuf,

    /// Canonical RAOP private-key file used to open the input object.
    #[arg(long, value_name = "PATH")]
    private_key: PathBuf,

    /// Canonical RAOR recipient public-key files, in ascending slot order.
    #[arg(long = "recipient", value_name = "PATH", num_args = 2..=8)]
    recipients: Vec<PathBuf>,

    /// New envelope object path; must not already exist.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Directory for the temporary plaintext; defaults adjacent to --out.
    #[arg(long, value_name = "DIR")]
    staging_dir: Option<PathBuf>,
}

/// Arguments for the shared `archive build` command.
#[derive(Args, Debug)]
struct ArchiveBuildArgs {
    /// Input files or directories. Directories are expanded recursively.
    #[arg(
        long = "inputs",
        value_name = "PATH",
        num_args = 1..,
        required_unless_present_any = ["scan_only", "map"]
    )]
    inputs: Vec<PathBuf>,

    /// Source-map TSV emitted by a trusted upstream planner.
    #[arg(
        long = "map",
        value_name = "PATH",
        conflicts_with_all = ["inputs", "rules", "scan_only"],
        requires = "source_root"
    )]
    map: Option<PathBuf>,

    /// Canonical source-root anchor for --map source paths.
    #[arg(long = "source-root", value_name = "DIR")]
    source_root: Option<PathBuf>,

    /// Expected SHA-256 of the source-map TSV.
    #[arg(long = "map-sha256", value_name = "HEX", requires = "map")]
    map_sha256: Option<String>,

    /// Output RAO object file.
    #[arg(
        long,
        value_name = "PATH",
        required_unless_present = "scan_only",
        conflicts_with = "scan_only"
    )]
    out: Option<PathBuf>,

    /// Ordered ingest ruleset for blob/exclude policy.
    #[arg(long, value_name = "PATH")]
    rules: Option<PathBuf>,

    /// Classify inputs and emit the clustered ingest report without writing RAO.
    #[arg(long)]
    scan_only: bool,

    /// Write the customer-facing member manifest JSON.
    #[arg(long, value_name = "PATH")]
    manifest_out: Option<PathBuf>,

    /// Disable generated `.remwrap.idx` siblings for blob wrappers.
    #[arg(long)]
    no_index: bool,

    /// Dense-subtree ratio threshold for blob suggestions.
    #[arg(long, value_name = "RATIO", default_value = "0.9")]
    blob_suggest_ratio: f64,

    /// Minimum non-compliant entries before suggesting a blob.
    #[arg(long, value_name = "COUNT", default_value = "100")]
    blob_suggest_count: u64,

    /// Non-compliant count above which scan reports a sanity-ceiling verdict.
    #[arg(long, value_name = "COUNT", default_value = "10000")]
    sanity_ceiling_count: u64,

    /// Canonical RAOR recipient public-key file; repeat 2 to 8 times.
    #[arg(long = "recipient", value_name = "RAOR")]
    recipients: Vec<PathBuf>,

    /// Object id to record in the RAO global header (default: fresh UUID).
    #[arg(long, value_name = "ID")]
    object_id: Option<String>,

    /// Opaque caller / orchestrator object id (default: object id).
    #[arg(long, value_name = "ID")]
    caller_object_id: Option<String>,

    /// Manifest file id (default: fresh UUID).
    #[arg(long, value_name = "ID")]
    manifest_file_id: Option<String>,

    /// RFC3339 write timestamp (default: current UTC time).
    #[arg(long, value_name = "RFC3339")]
    timestamp: Option<String>,

    /// RAO object block/chunk size.
    #[arg(long, value_name = "BYTES", value_parser = parse_archive_chunk_size, default_value = "256KiB")]
    chunk_size: usize,
}

/// Arguments for the shared `archive inspect` command.
#[derive(Args, Debug)]
struct ArchiveInspectArgs {
    /// Input RAO object file.
    #[arg(long, value_name = "PATH")]
    object: PathBuf,

    /// RAO object block/chunk size for plaintext objects.
    #[arg(long, value_name = "BYTES", value_parser = parse_archive_chunk_size, default_value = "256KiB")]
    chunk_size: usize,
}

/// Arguments for the shared `archive extract` command.
#[derive(Args, Debug)]
struct ArchiveExtractArgs {
    /// Input RAO object file.
    #[arg(long, value_name = "PATH")]
    object: PathBuf,

    /// Destination directory.
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,

    /// RAO object block/chunk size for plaintext objects.
    #[arg(long, value_name = "BYTES", value_parser = parse_archive_chunk_size, default_value = "256KiB")]
    chunk_size: usize,

    /// Canonical RAOP private-key file. Required for encrypted objects.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Archive member path for byte-range extraction.
    #[arg(long = "path", value_name = "RAO_PATH")]
    path: Option<String>,

    /// First object-local BodyLba for --path, from the build report or catalog row.
    #[arg(long = "first-chunk-lba", value_name = "LBA")]
    first_chunk_lba: Option<u64>,

    /// Full member size for --path, from the build report or catalog row.
    #[arg(long = "file-size-bytes", value_name = "BYTES", value_parser = parse_archive_file_size_bytes)]
    file_size_bytes: Option<u64>,

    /// Member byte range to extract, formatted as start:length.
    #[arg(long = "range", value_name = "START:LEN", value_parser = parse_archive_byte_range)]
    range: Option<ArchiveByteRange>,

    /// Replace existing destination files.
    #[arg(long)]
    overwrite: bool,

    /// Keep `.remwrap.tar` and `.remwrap.idx` entries literal instead of unwrapping.
    #[arg(long)]
    no_unwrap: bool,

    /// RAO blob wrapper entry to use for single-member restore.
    #[arg(long = "blob-entry", value_name = "RAO_PATH", requires = "blob_member")]
    blob_entry: Option<String>,

    /// Member path inside --blob-entry to restore.
    #[arg(long = "blob-member", value_name = "TAR_PATH", requires = "blob_entry")]
    blob_member: Option<String>,
}

/// Arguments for the shared `archive extract-stream` command.
#[derive(Args, Debug)]
struct ArchiveExtractStreamArgs {
    /// Canonical RAOP private-key file used to decrypt the envelope.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Absolute plaintext byte range to write, formatted as start:length.
    #[arg(long = "range", value_name = "START:LEN", value_parser = parse_archive_byte_range)]
    range: Option<ArchiveByteRange>,

    /// Authenticated header+metadata prefix for ranged-ciphertext input.
    #[arg(
        long,
        value_name = "PATH",
        requires = "stored_range_start",
        requires = "range"
    )]
    authenticated_prefix: Option<PathBuf>,

    /// Absolute stored offset at which ranged ciphertext stdin begins.
    #[arg(
        long,
        value_name = "BYTE",
        requires = "authenticated_prefix",
        requires = "range"
    )]
    stored_range_start: Option<u64>,
}

/// Arguments for the shared `archive covering-range` command.
#[derive(Args, Debug)]
struct ArchiveCoveringRangeArgs {
    /// Canonical RAOP private-key file used to authenticate the envelope.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Expected encrypted object identifier.
    #[arg(long, value_name = "ID")]
    object_id: String,

    /// Caller member identifier, echoed in the machine-readable response.
    #[arg(long, value_name = "ID")]
    file_id: String,

    /// Absolute plaintext member range, formatted as start:length.
    #[arg(long = "range", value_name = "START:LEN", value_parser = parse_archive_byte_range)]
    range: ArchiveByteRange,
}

/// Arguments for the shared `archive write` command (post-From transform).
#[derive(Args, Debug)]
struct ArchiveWriteArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Local file to write.
    #[arg(long, value_name = "PATH")]
    file: PathBuf,

    /// Tape pool to target. Field is `pool_id` internally; the CLI flag
    /// is `--pool` to match the operator-facing `rem` surface.
    #[arg(long = "pool", value_name = "POOL")]
    pool_id: String,

    /// Override the in-archive path (default: file basename).
    #[arg(long, value_name = "PATH")]
    archive_path: Option<PathBuf>,

    /// Opaque caller / orchestrator object id (default: fresh UUID).
    #[arg(long, value_name = "ID")]
    caller_object_id: Option<String>,

    /// Canonical RAOR recipient public-key file; repeat 2 to 8 times.
    #[arg(long = "recipient", value_name = "RAOR")]
    recipients: Vec<PathBuf>,

    /// Emit the locator as one JSON line to stdout (seam contract §4).
    #[arg(long = "json")]
    json_output: bool,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}

/// Arguments for the shared `archive read` command (post-From transform).
#[derive(Args, Debug)]
struct ArchiveReadArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Destination path for the restored payload bytes.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Canonical RAOP private-key file. Required for encrypted copies.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}

/// Arguments for the shared `archive export-object` command.
#[derive(Args, Debug)]
struct ArchiveExportObjectArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Destination path for the complete stored object bytes.
    #[arg(long, value_name = "PATH")]
    out: PathBuf,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}

/// Arguments for the shared `archive verify` command (post-From transform).
#[derive(Args, Debug)]
struct ArchiveVerifyArgs {
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Expected payload SHA-256 (hex) to compare the tape bytes against.
    #[arg(long, value_name = "HEX")]
    expected_sha256: String,

    /// Canonical RAOP private-key file. Required for encrypted copies.
    #[arg(long, value_name = "PATH")]
    private_key: Option<PathBuf>,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}

/// Arguments for the shared `archive list` command (post-From transform).
#[derive(Args, Debug)]
struct ArchiveListArgs {
    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
}

#[derive(Args, Debug)]
struct ArchiveInputArgs {
    /// Archive format driver.
    #[arg(long, value_enum)]
    format: ArchiveFormat,

    #[command(flatten)]
    source: ArchiveSourceArgs,
}

#[derive(Args, Debug)]
struct ArchiveRestoreArgs {
    /// Archive format driver.
    #[arg(long, value_enum)]
    format: ArchiveFormat,

    #[command(flatten)]
    source: ArchiveSourceArgs,

    /// Destination directory.
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,

    /// Replace existing destination files.
    #[arg(long)]
    overwrite: bool,
}

#[derive(Args, Debug)]
struct ArchiveRecoverArgs {
    /// Archive format driver.
    #[arg(long, value_enum)]
    format: ArchiveFormat,

    #[command(flatten)]
    source: ArchiveSourceArgs,

    /// Destination directory.
    #[arg(long, value_name = "DIR")]
    dest: PathBuf,
}

#[derive(Args, Debug)]
struct ArchiveSourceArgs {
    /// Path to a byte-stream archive dump.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "tape",
        required_unless_present = "tape"
    )]
    dump: Option<PathBuf>,

    /// Library serial for a tape source.
    #[arg(
        long,
        value_name = "SERIAL",
        conflicts_with = "dump",
        required_unless_present = "dump",
        requires = "bay"
    )]
    tape: Option<String>,

    /// Drive bay containing the tape.
    #[arg(long, value_parser = parse_element_addr, requires = "tape")]
    bay: Option<u16>,

    /// Rewind the tape before probing, scanning, or restoring.
    #[arg(long, requires = "tape")]
    rewind: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ArchiveFormat {
    /// BRU/BRU-PE legacy archive.
    Bru,
}

impl ArchiveCommand {
    fn tape_target(&self) -> Option<&str> {
        match self {
            Self::Capabilities
            | Self::Reseal(_)
            | Self::Build(_)
            | Self::Inspect(_)
            | Self::Extract(_)
            | Self::ExtractStream(_) => None,
            // `archive write` is state-changing; gate it through `--allow`
            // against the supplied `--library` serial.
            Self::Write(args) => Some(args.library.as_str()),
            Self::Read(args) => Some(args.library.as_str()),
            Self::ExportObject(args) => Some(args.library.as_str()),
            Self::Verify(args) => Some(args.library.as_str()),
            // `archive list` reads only the catalog — no tape, no --allow gate.
            Self::List(_) => None,
            _ => self.source().tape.as_deref(),
        }
    }

    fn is_dump_command(&self) -> bool {
        match self {
            Self::Build(_)
            | Self::Capabilities
            | Self::Reseal(_)
            | Self::Inspect(_)
            | Self::Extract(_)
            | Self::ExtractStream(_)
            | Self::Write(_)
            | Self::Read(_)
            | Self::ExportObject(_)
            | Self::Verify(_)
            | Self::List(_) => false,
            _ => self.source().dump.is_some(),
        }
    }

    fn is_pool_write_command(&self) -> bool {
        matches!(self, Self::Write(_))
    }

    fn source(&self) -> &ArchiveSourceArgs {
        match self {
            Self::Probe(args) | Self::Scan(args) => &args.source,
            Self::Restore(args) => &args.source,
            Self::Recover(args) => &args.source,
            Self::Build(_) => panic!("ArchiveCommand::Build has no dump/tape source"),
            Self::Capabilities => panic!("ArchiveCommand::Capabilities has no dump/tape source"),
            Self::Reseal(_) => panic!("ArchiveCommand::Reseal has no dump/tape source"),
            Self::Inspect(_) => panic!("ArchiveCommand::Inspect has no dump/tape source"),
            Self::Extract(_) => panic!("ArchiveCommand::Extract has no dump/tape source"),
            Self::ExtractStream(_) => {
                panic!("ArchiveCommand::ExtractStream has no dump/tape source")
            }
            Self::CoveringRange(_) => {
                panic!("ArchiveCommand::CoveringRange has no dump/tape source")
            }
            Self::Write(_) => panic!("ArchiveCommand::Write has no dump/tape source"),
            Self::Read(_) => panic!("ArchiveCommand::Read has no dump/tape source"),
            Self::ExportObject(_) => {
                panic!("ArchiveCommand::ExportObject has no dump/tape source")
            }
            Self::Verify(_) => panic!("ArchiveCommand::Verify has no dump/tape source"),
            Self::List(_) => panic!("ArchiveCommand::List has no dump/tape source"),
        }
    }

    fn format(&self) -> ArchiveFormat {
        match self {
            Self::Probe(args) | Self::Scan(args) => args.format,
            Self::Restore(args) => args.format,
            Self::Recover(args) => args.format,
            Self::Build(_) => panic!("ArchiveCommand::Build has no format"),
            Self::Capabilities => panic!("ArchiveCommand::Capabilities has no format"),
            Self::Reseal(_) => panic!("ArchiveCommand::Reseal has no format"),
            Self::Inspect(_) => panic!("ArchiveCommand::Inspect has no format"),
            Self::Extract(_) => panic!("ArchiveCommand::Extract has no format"),
            Self::ExtractStream(_) => panic!("ArchiveCommand::ExtractStream has no format"),
            Self::CoveringRange(_) => panic!("ArchiveCommand::CoveringRange has no format"),
            Self::Write(_) => panic!("ArchiveCommand::Write has no format"),
            Self::Read(_) => panic!("ArchiveCommand::Read has no format"),
            Self::ExportObject(_) => panic!("ArchiveCommand::ExportObject has no format"),
            Self::Verify(_) => panic!("ArchiveCommand::Verify has no format"),
            Self::List(_) => panic!("ArchiveCommand::List has no format"),
        }
    }
}

impl ArchiveSourceArgs {
    fn selection(&self) -> Result<ArchiveSourceSelection<'_>, String> {
        match (&self.dump, &self.tape) {
            (Some(path), None) => Ok(ArchiveSourceSelection::Dump(path.as_path())),
            (None, Some(serial)) => {
                let bay = self
                    .bay
                    .ok_or_else(|| "archive tape source requires --bay".to_string())?;
                Ok(ArchiveSourceSelection::Tape {
                    serial,
                    bay,
                    rewind: self.rewind,
                })
            }
            (None, None) => Err("archive source requires --dump or --tape".to_string()),
            (Some(_), Some(_)) => Err("archive source accepts only one of --dump or --tape".into()),
        }
    }
}

enum ArchiveSourceSelection<'a> {
    Dump(&'a Path),
    Tape {
        serial: &'a str,
        bay: u16,
        rewind: bool,
    },
}

impl ArchiveFormat {
    fn cli_name(self) -> &'static str {
        match self {
            Self::Bru => "bru",
        }
    }

    fn driver_id(self) -> &'static str {
        match self {
            Self::Bru => "remanence-bru",
        }
    }
}

pub fn main_entry() -> ExitCode {
    let cli = Cli::parse();
    run_cli(cli.into(), CliMode::Rem)
}

pub fn debug_main_entry() -> ExitCode {
    let cli = DebugCli::parse();
    run_cli(cli.into(), CliMode::Debug)
}

#[cfg(target_os = "linux")]
fn cli_discover() -> Result<DiscoveryReport, DiscoveryError> {
    let factory = cli_transport_factory(CliTransportAccess::ReadOnly)
        .map_err(|cause| DiscoveryError::EnumerationDenied { cause })?;
    let devices = remanence_library::sysfs::enumerate_sg_devices()
        .map_err(|cause| DiscoveryError::EnumerationDenied { cause })?;
    remanence_library::discover_with(devices, factory)
}

#[cfg(target_os = "linux")]
pub(crate) fn open_library_handle(
    library: &Library,
    policy: &dyn remanence_library::AccessPolicy,
) -> Result<remanence_library::LibraryHandle, OpenError> {
    let factory = cli_transport_factory(CliTransportAccess::ReadWrite).map_err(|cause| {
        OpenError::DeviceUnavailable {
            path: library.changer_sg.clone(),
            cause,
        }
    })?;
    library.open_with(policy, factory)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn open_library_handle(
    library: &Library,
    _policy: &dyn remanence_library::AccessPolicy,
) -> Result<remanence_library::LibraryHandle, OpenError> {
    Err(OpenError::DeviceUnavailable {
        path: library.changer_sg.clone(),
        cause: IoErrorKind {
            kind: "Unsupported",
            message: "library hardware access is only implemented on Linux".to_string(),
            raw_os_error: None,
        },
    })
}

#[cfg(target_os = "linux")]
fn cli_transport_factory(access: CliTransportAccess) -> Result<CliTransportFactory, IoErrorKind> {
    let engine = if remanence_chaos::chaos_real_enabled_from_env() {
        Some(remanence_chaos::FaultEngine::from_env().map_err(chaos_io_error)?)
    } else {
        None
    };

    Ok(Box::new(move |path| {
        let inner = match access {
            CliTransportAccess::ReadOnly => remanence_library::LinuxSgTransport::open(path),
            CliTransportAccess::ReadWrite => remanence_library::LinuxSgTransport::open_rw(path),
        }
        .map_err(|error| IoErrorKind::from(&error))?;

        if let Some(engine) = engine.clone() {
            Ok(Box::new(remanence_chaos::ChaosTransport::new(
                inner,
                engine,
                chaos_device_ctx(path),
            )) as Box<dyn remanence_library::SgTransport>)
        } else {
            Ok(Box::new(inner) as Box<dyn remanence_library::SgTransport>)
        }
    }))
}

#[cfg(target_os = "linux")]
fn chaos_device_ctx(path: &Path) -> remanence_chaos::DeviceCtx {
    let label = path
        .file_name()
        .and_then(OsStr::to_str)
        .map(str::to_owned)
        .unwrap_or_else(|| path.display().to_string());
    let drive_id = label
        .strip_prefix("sg")
        .and_then(|suffix| suffix.parse::<u16>().ok())
        .map(|index| format!("drive{}", u32::from(index) + 1))
        .unwrap_or(label);
    remanence_chaos::DeviceCtx::new()
        .with_backend("linux")
        .with_drive_id(drive_id)
}

#[cfg(target_os = "linux")]
fn chaos_io_error(error: remanence_chaos::ChaosError) -> IoErrorKind {
    IoErrorKind {
        kind: "Other",
        message: format!("chaos runtime: {error}"),
        raw_os_error: None,
    }
}

fn run_cli(cli: ParsedCli, mode: CliMode) -> ExitCode {
    #[cfg(target_os = "linux")]
    let discover_fn = move || {
        if mode == CliMode::Debug {
            cli_discover()
        } else {
            remanence_library::discover()
        }
    };
    #[cfg(not(target_os = "linux"))]
    let discover_fn: fn() -> Result<DiscoveryReport, DiscoveryError> = || {
        Err(DiscoveryError::EnumerationDenied {
            cause: remanence_library::IoErrorKind {
                kind: "Unsupported",
                message: "device discovery is only implemented on Linux".to_string(),
                raw_os_error: None,
            },
        })
    };
    let stdout = io::stdout();
    let stderr = io::stderr();
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    run_with_mode(cli, mode, discover_fn, &mut input, &mut out, &mut err)
}

/// Core entry-point — generic over the discovery function and the
/// stdout/stderr writers so tests can inject a synthetic
/// `DiscoveryReport` and capture every byte we'd print. Returns the
/// exit code rather than printing-and-exiting so the test can assert
/// on it.
#[cfg(test)]
fn run<F>(cli: Cli, discover_fn: F, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode
where
    F: FnOnce() -> Result<DiscoveryReport, DiscoveryError>,
{
    run_with_mode(
        cli.into(),
        CliMode::Rem,
        discover_fn,
        &mut io::empty(),
        out,
        err,
    )
}

#[cfg(test)]
fn run_debug<F>(cli: DebugCli, discover_fn: F, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode
where
    F: FnOnce() -> Result<DiscoveryReport, DiscoveryError>,
{
    run_with_mode(
        cli.into(),
        CliMode::Debug,
        discover_fn,
        &mut io::empty(),
        out,
        err,
    )
}

fn run_with_mode<F>(
    cli: ParsedCli,
    mode: CliMode,
    discover_fn: F,
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode
where
    F: FnOnce() -> Result<DiscoveryReport, DiscoveryError>,
{
    if mode == CliMode::Rem {
        if let Some(reason) = rem_debug_only_reason(&cli.command) {
            let _ = writeln!(
                err,
                "error: {reason} is available through `rem-debug`, not `rem`"
            );
            let _ = writeln!(
                err,
                "       use `rem-debug` only for break-glass local hardware/debug work; normal production paths must go through the daemon"
            );
            return ExitCode::from(1);
        }
    } else if let Some(reason) = rem_only_reason(&cli.command) {
        let _ = writeln!(
            err,
            "error: {reason} are available through `rem`, not `rem-debug`"
        );
        return ExitCode::from(1);
    }

    match &cli.command {
        Command::DaemonClient {
            endpoint,
            json,
            command,
        } => return run_daemon_client_command(endpoint, *json, command, out, err),
        Command::OperationClient {
            endpoint,
            json,
            command,
        } => return run_operation_client_command(endpoint, *json, command, out, err),
        Command::CatalogClient {
            endpoint,
            json,
            command,
        } => return run_catalog_client_command(endpoint, *json, command, out, err),
        Command::AuditClient {
            endpoint,
            json,
            command,
        } => return run_audit_client_command(endpoint, *json, command, out, err),
        Command::DriveClient {
            endpoint,
            json,
            command,
        } => return run_drive_client_command(endpoint, *json, command, out, err),
        Command::AlarmsClient {
            endpoint,
            json,
            all,
            command,
        } => return run_alarms_client_command(endpoint, *json, *all, command, out, err),
        Command::Top {
            endpoint,
            json,
            once,
        } => return run_top_command(endpoint, *json, *once, out, err),
        Command::TapeAlertsAlias {
            endpoint,
            json,
            drive,
        } => {
            let command = DriveClientCommand::Alerts {
                drive: drive.clone(),
            };
            return run_drive_client_command(endpoint, *json, &command, out, err);
        }
        Command::ArchiveVerifyClient {
            endpoint,
            library,
            drive,
            locator,
            expected_sha256,
        } => {
            return run_archive_verify_client(
                endpoint,
                library,
                *drive,
                locator,
                expected_sha256,
                out,
                err,
            );
        }
        Command::Libraries { .. }
        | Command::Library { .. }
        | Command::Move { .. }
        | Command::Load { .. }
        | Command::Unload { .. }
        | Command::Export { .. }
        | Command::Import { .. }
        | Command::Rescan { .. }
        | Command::Lock { .. }
        | Command::Unlock { .. }
        | Command::Watch { .. }
        | Command::RebuildCatalogFromJournals { .. }
        | Command::Catalog { .. }
        | Command::Tape { .. }
        | Command::Archive { .. }
        | Command::Dev { .. } => {}
    }

    // Pre-discovery allowlist gate. State-changing subcommands
    // require the target library on `--allow`; checking here lets
    // the operator who forgot the flag see the error WITHOUT
    // touching /dev/sg* first (no SCSI probes, no CAP_SYS_RAWIO
    // surprises, no audit-log noise for an op that wasn't going to
    // run anyway).
    if let Some(target) = state_changing_target(&cli.command) {
        if !cli.allow.iter().any(|s| s == target) {
            let _ = writeln!(
                err,
                "error: library {target:?} not on the --allow list — \
                 state-changing ops are refused"
            );
            let _ = writeln!(
                err,
                "       pass `--allow {target}` to permit this invocation"
            );
            return ExitCode::from(1);
        }
    }
    if let Command::Dev { command } = &cli.command {
        if let Err(error) = command.validate_before_discovery() {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
    }
    if let Command::Tape { command } = &cli.command {
        if let Err(error) = command.validate_before_discovery() {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
        // These tape subcommands are catalog + audit only — no SCSI, no
        // library allowlist — so they bypass discovery entirely (like the
        // catalog maintenance commands above).
        match command {
            TapeCommand::Retire(args) => return run_tape_retire(args, out, err),
            TapeCommand::Quarantine { command } => {
                return run_tape_quarantine(command, out, err);
            }
            _ => {}
        }
    }

    // `rem watch` bypasses discovery entirely — it is purely
    // notification-driven and never issues SCSI.
    if let Command::Watch { coalesce_window } = &cli.command {
        return run_watch(coalesce_window, out, err);
    }
    if let Command::RebuildCatalogFromJournals { config } = &cli.command {
        return run_rebuild_catalog_from_journals(config, out, err);
    }
    if let Command::Catalog { command } = &cli.command {
        return run_local_catalog_command(command, out, err);
    }
    if let Command::Archive { command } = &cli.command {
        let command = command.as_ref();
        if matches!(command, ArchiveCommand::Capabilities) {
            return run_archive_capabilities(out, err);
        }
        if let ArchiveCommand::Reseal(args) = command {
            return run_archive_reseal(args, out, err);
        }
        if let ArchiveCommand::Build(args) = command {
            return run_archive_build(args, out, err);
        }
        if let ArchiveCommand::Inspect(args) = command {
            return run_archive_inspect(args, out, err);
        }
        if let ArchiveCommand::Extract(args) = command {
            return run_archive_extract(args, out, err);
        }
        if let ArchiveCommand::ExtractStream(args) = command {
            return run_archive_extract_stream(args, input, out, err);
        }
        if let ArchiveCommand::CoveringRange(args) = command {
            return run_archive_covering_range(args, input, out, err);
        }
        if command.is_dump_command() {
            return run_archive_dump_command(command, out, err);
        }
        // `archive list` is a catalog read — no SCSI, so bypass discovery
        // entirely (like rebuild-catalog-from-journals above).
        if let ArchiveCommand::List(args) = command {
            return pool_ops::run_archive_list(
                &pool_ops::ArchiveListArgs {
                    config: args.config.clone(),
                },
                out,
                err,
            );
        }
    }

    let report = match discover_fn() {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            // Errors that carry warnings (NoLibraries when devices
            // were seen but every probe failed) should surface those
            // warnings so the operator can see the root cause —
            // most notably EPERM from a missing CAP_SYS_RAWIO.
            if let DiscoveryError::NoLibraries { warnings } = &e {
                if !warnings.is_empty() {
                    print_warning_list(warnings, err);
                    print_setcap_hint_if_needed(warnings, err);
                }
            }
            return ExitCode::from(1);
        }
    };
    let allow = cli.allow.clone();
    let allow_derived = cli.allow_derived.clone();
    match cli.command {
        Command::Libraries { json } => {
            if json {
                print_libraries_json(&report, out);
            } else {
                print_libraries(&report, out);
            }
        }
        Command::Library {
            serial,
            slots,
            json,
        } => match report.library(&serial) {
            Some(lib) => {
                if json {
                    print_library_json(lib, slots, out);
                } else {
                    print_library(lib, &report, out);
                }
                if slots && !json {
                    print_slots(lib, out);
                }
            }
            None => {
                let _ = writeln!(err, "error: no library with serial {serial:?} on this host");
                let _ = writeln!(err, "       run `rem libraries` to see what's available");
                print_warnings(&report, err);
                return ExitCode::from(2);
            }
        },
        Command::TapeAlertsAlias { .. } => unreachable!("daemon alias dispatched pre-discovery"),
        Command::Move { serial, src, dst } => {
            return run_state_change(
                &report,
                &serial,
                &allow,
                &allow_derived,
                out,
                err,
                |handle, policy| {
                    handle
                        .move_medium(src, dst, policy)
                        .map(|()| format!("moved 0x{src:04x} → 0x{dst:04x}"))
                        .map_err(|e| e.to_string())
                },
            );
        }
        Command::Load { serial, slot, bay } => {
            return run_state_change(
                &report,
                &serial,
                &allow,
                &allow_derived,
                out,
                err,
                |handle, policy| {
                    handle
                        .load(slot, bay, policy)
                        .map(|()| format!("loaded slot 0x{slot:04x} → bay 0x{bay:04x}"))
                        .map_err(|e| e.to_string())
                },
            );
        }
        Command::Unload { serial, bay, dest } => {
            return run_state_change(
                &report,
                &serial,
                &allow,
                &allow_derived,
                out,
                err,
                |handle, policy| {
                    handle
                        .unload(bay, dest, policy)
                        .map(|()| {
                            let where_to = match dest {
                                Some(d) => format!("→ slot 0x{d:04x}"),
                                None => "→ recorded source slot".to_string(),
                            };
                            format!("unloaded bay 0x{bay:04x} {where_to}")
                        })
                        .map_err(|e| e.to_string())
                },
            );
        }
        Command::Export { serial, slot } => {
            return run_state_change(
                &report,
                &serial,
                &allow,
                &allow_derived,
                out,
                err,
                |handle, policy| {
                    handle
                        .export(slot, policy)
                        .map(|()| {
                            // Don't promise the cartridge is parked in
                            // an IE element — vendors differ (HPE parks
                            // visibly, QuadStor vaults). The
                            // dirty-snapshot recovery hint that follows
                            // tells the operator how to confirm.
                            format!("export issued for slot 0x{slot:04x}")
                        })
                        .map_err(|e| e.to_string())
                },
            );
        }
        Command::Import { serial, slot } => {
            return run_state_change(
                &report,
                &serial,
                &allow,
                &allow_derived,
                out,
                err,
                |handle, policy| {
                    handle
                        .import(slot, policy)
                        .map(|()| {
                            // Mirror `export`: the dirty-snapshot
                            // recovery hint covers the post-state
                            // (some vendors expose the source as a
                            // vault rather than a true IE port).
                            format!("import issued for slot 0x{slot:04x}")
                        })
                        .map_err(|e| e.to_string())
                },
            );
        }
        Command::Rescan { serial } => {
            return run_state_change(
                &report,
                &serial,
                &allow,
                &allow_derived,
                out,
                err,
                |handle, _policy| {
                    handle
                        .rescan()
                        .map(|()| "rescan ok".to_string())
                        .map_err(|e| e.to_string())
                },
            );
        }
        Command::Lock { serial } => {
            let serial_for_hint = serial.clone();
            return run_state_change(
                &report,
                &serial,
                &allow,
                &allow_derived,
                out,
                err,
                |handle, _policy| {
                    // The guard would drop and immediately re-issue
                    // ALLOW, so call lock_removal and then forget the
                    // guard. The operator follows up with `rem-debug unlock`
                    // when their critical section is over.
                    match handle.lock_removal() {
                        Ok(guard) => {
                            std::mem::forget(guard);
                            Ok(format!(
                                "locked — call `rem-debug unlock {serial_for_hint} \
                                 --allow {serial_for_hint}` when done"
                            ))
                        }
                        Err(e) => Err(e.to_string()),
                    }
                },
            );
        }
        Command::Unlock { serial } => {
            return run_state_change(
                &report,
                &serial,
                &allow,
                &allow_derived,
                out,
                err,
                |handle, _policy| {
                    handle
                        .allow_removal()
                        .map(|()| "unlocked".to_string())
                        .map_err(|e| e.to_string())
                },
            );
        }
        // Watch is dispatched before discovery; reaching here means a
        // bug in the early-return logic above.
        Command::Watch { .. } => unreachable!("rem watch dispatched pre-discovery"),
        Command::RebuildCatalogFromJournals { .. } => {
            unreachable!("catalog rebuild dispatched pre-discovery")
        }
        Command::Catalog { .. } => unreachable!("catalog maintenance dispatched pre-discovery"),
        Command::DaemonClient { .. }
        | Command::OperationClient { .. }
        | Command::CatalogClient { .. }
        | Command::DriveClient { .. }
        | Command::AlarmsClient { .. } => {
            unreachable!("daemon client command dispatched pre-discovery")
        }
        Command::Tape { command } => {
            return run_tape_command(&report, &command, out, err);
        }
        Command::Archive { command } => {
            let command = command.as_ref();
            if command.is_pool_write_command() {
                if let ArchiveCommand::Write(args) = command {
                    return pool_ops::run_archive_write(
                        &report,
                        &pool_ops::ArchiveWriteArgs {
                            library: args.library.clone(),
                            file: args.file.clone(),
                            pool_id: args.pool_id.clone(),
                            archive_path: args.archive_path.clone(),
                            caller_object_id: args.caller_object_id.clone(),
                            recipients: args.recipients.clone(),
                            json_output: args.json_output,
                            config: args.config.clone(),
                        },
                        &allow,
                        &allow_derived,
                        out,
                        err,
                    );
                }
            }
            if let ArchiveCommand::Read(args) = command {
                return pool_ops::run_archive_read(
                    &report,
                    &pool_ops::ArchiveReadArgs {
                        library: args.library.clone(),
                        locator: args.locator.clone(),
                        out: args.out.clone(),
                        private_key: args.private_key.clone(),
                        config: args.config.clone(),
                    },
                    &allow,
                    &allow_derived,
                    out,
                    err,
                );
            }
            if let ArchiveCommand::ExportObject(args) = command {
                return pool_ops::run_archive_export_object(
                    &report,
                    &pool_ops::ArchiveExportObjectArgs {
                        library: args.library.clone(),
                        locator: args.locator.clone(),
                        out: args.out.clone(),
                        config: args.config.clone(),
                    },
                    &allow,
                    &allow_derived,
                    out,
                    err,
                );
            }
            if let ArchiveCommand::Verify(args) = command {
                return pool_ops::run_archive_verify(
                    &report,
                    &pool_ops::ArchiveVerifyArgs {
                        library: args.library.clone(),
                        locator: args.locator.clone(),
                        expected_sha256: args.expected_sha256.clone(),
                        private_key: args.private_key.clone(),
                        config: args.config.clone(),
                    },
                    &allow,
                    &allow_derived,
                    out,
                    err,
                );
            }
            return run_archive_tape_command(&report, command, &allow, &allow_derived, out, err);
        }
        Command::Dev { command } => {
            return run_dev_command(&report, &command, &allow, &allow_derived, out, err);
        }
        Command::Top { .. } | Command::AuditClient { .. } | Command::ArchiveVerifyClient { .. } => {
            unreachable!("daemon client command returns before discovery")
        }
    }
    print_warnings(&report, err);
    ExitCode::SUCCESS
}

fn run_archive_capabilities(out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let capabilities = json!({
        "capabilities": [
            "rao-envelope",
            "wrap-suite-hpke-v1",
            "ranged-ciphertext-extract"
        ]
    });
    match serde_json::to_writer(&mut *out, &capabilities)
        .and_then(|()| writeln!(out).map_err(serde_json::Error::io))
    {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(err, "error: write archive capabilities: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_archive_reseal(
    args: &ArchiveResealArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    run_archive_reseal_with(args, out, err, reseal_archive_object)
}

fn run_archive_reseal_with<F>(
    args: &ArchiveResealArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
    operation: F,
) -> ExitCode
where
    F: FnOnce(&ArchiveResealArgs) -> Result<PublishedReseal, String>,
{
    match operation(args) {
        Ok(publication) => match serde_json::to_writer(&mut *out, &publication.report)
            .and_then(|()| writeln!(out).map_err(serde_json::Error::io))
        {
            Ok(()) => {
                publication.commit();
                ExitCode::SUCCESS
            }
            Err(error) => {
                let _ = writeln!(err, "error: write reseal report: {error}");
                ExitCode::from(1)
            }
        },
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            ExitCode::from(1)
        }
    }
}

/// Armed publication whose output is removed unless report emission commits it.
struct PublishedReseal {
    report: Value,
    output: PathBuf,
    #[cfg(unix)]
    output_identity: (u64, u64),
    armed: bool,
}

impl PublishedReseal {
    fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for PublishedReseal {
    fn drop(&mut self) {
        // Documented limitation: the still_owns_output() stat and remove_file are
        // not atomic — a sub-ms TOCTOU exists if a concurrent actor unlinks and
        // recreates --out in that window. Accepted under the single-operator
        // reseal threat model, and strictly safer than an unconditional delete.
        if self.armed && self.still_owns_output() {
            let _ = fs::remove_file(&self.output);
        }
    }
}

impl PublishedReseal {
    #[cfg(unix)]
    fn still_owns_output(&self) -> bool {
        use std::os::unix::fs::MetadataExt;

        fs::metadata(&self.output)
            .map(|metadata| (metadata.dev(), metadata.ino()) == self.output_identity)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    fn still_owns_output(&self) -> bool {
        self.output.exists()
    }
}

fn reseal_archive_object(args: &ArchiveResealArgs) -> Result<PublishedReseal, String> {
    reseal_archive_object_with_verifier(args, |path, expected| {
        let actual = sha256_file(path)?;
        if actual != expected {
            return Err("staged encrypted object hash differs from the sealer report".to_string());
        }
        Ok(actual)
    })
}

fn reseal_archive_object_with_verifier<F>(
    args: &ArchiveResealArgs,
    verify_staged: F,
) -> Result<PublishedReseal, String>
where
    F: FnOnce(&Path, [u8; 32]) -> Result<[u8; 32], String>,
{
    if args.out.exists() {
        return Err(format!("--out {} already exists", args.out.display()));
    }
    let mut encrypted = File::open(&args.object)
        .map_err(|error| format!("open encrypted object {}: {error}", args.object.display()))?;
    let mut header_bytes = [0u8; RAO_HEADER_LEN];
    encrypted
        .read_exact(&mut header_bytes)
        .map_err(|error| format!("read encrypted object header: {error}"))?;
    let input_header = RaoHeader::parse(&header_bytes)
        .map_err(|error| format!("parse encrypted object header: {error}"))?;
    encrypted
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("rewind encrypted object: {error}"))?;
    let recipients = read_recipient_public_key_files(&args.recipients)?;
    let staging_dir = args
        .staging_dir
        .as_deref()
        .or_else(|| {
            args.out
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
        })
        .unwrap_or_else(|| Path::new("."));
    let mut plaintext = SecurePlaintextStage::new_in(staging_dir)?;
    let private_key = read_private_key_file(&args.private_key)?;
    let opened = remanence_format::open_envelope_rao_stream(
        &mut encrypted,
        plaintext.as_file_mut(),
        &private_key,
    )
    .map_err(|error| format!("open encrypted object: {error}"))?;
    plaintext
        .as_file_mut()
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("rewind secure plaintext staging file: {error}"))?;

    let seal_options = EnvelopeSealOptions {
        common: SealOptions {
            chunk_size: opened.header.chunk_size,
            object_id: opened.header.object_id.clone(),
            plaintext_size: opened.metadata.plaintext_size,
            plaintext_digest: opened.metadata.plaintext_digest,
        },
        recipients,
    };

    let temp = temporary_archive_output_path(&args.out);
    if temp.exists() {
        return Err(format!(
            "temporary output {} already exists",
            temp.display()
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| format!("create {}: {error}", temp.display()))?;
    let report = match remanence_format::seal_envelope_rao_stream(
        plaintext.as_file_mut(),
        &mut file,
        &seal_options,
    ) {
        Ok(report) => report,
        Err(error) => {
            drop(file);
            let _ = fs::remove_file(&temp);
            return Err(format!("seal encrypted object: {error}"));
        }
    };
    if let Err(error) = file.sync_all() {
        drop(file);
        let _ = fs::remove_file(&temp);
        return Err(format!("sync {}: {error}", temp.display()));
    }
    #[cfg(unix)]
    let output_identity = {
        use std::os::unix::fs::MetadataExt;

        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(&temp);
                return Err(format!("stat {}: {error}", temp.display()));
            }
        };
        (metadata.dev(), metadata.ino())
    };
    drop(file);
    let write_result: Result<Value, String> = (|| {
        let staged_digest = verify_staged(&temp, report.stored_digest)?;
        let value = json!({
            "input": args.object,
            "output": args.out,
            "object_id": report.header.object_id,
            "chunk_size": report.header.chunk_size,
            "plaintext_digest": bytes_to_hex(&report.plaintext.digest),
            "input_format_version": input_header.format_version,
            "output_format_version": 2,
            "recipient_epochs": recipient_epochs_json(&report.key_frame),
            "stored_size_bytes": report.stored_size_bytes,
            "expected_sha256": bytes_to_hex(&report.stored_digest),
            "published_sha256": bytes_to_hex(&staged_digest),
            "verified_after_write": true
        });
        publish_noreplace(&temp, &args.out).map_err(|error| {
            format!(
                "publish resealed object {} -> {}: {error}",
                temp.display(),
                args.out.display()
            )
        })?;
        Ok(value)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    Ok(PublishedReseal {
        report: write_result?,
        output: args.out.clone(),
        #[cfg(unix)]
        output_identity,
        armed: true,
    })
}

/// Publishes without replacing a destination created by a racing process.
fn publish_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let source = CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "source path contains NUL"))?;
        let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "destination path contains NUL")
        })?;
        // SAFETY: both pointers are valid NUL-terminated path strings for the
        // duration of the syscall, and no mutable memory is shared with it.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                destination.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::ENOSYS) | Some(libc::EINVAL) | Some(libc::EOPNOTSUPP)
        ) {
            return Err(error);
        }
    }

    // Portable fallback for kernels without renameat2 support (ENOSYS/EINVAL/
    // EOPNOTSUPP). hard_link is an atomic O_EXCL-style create (fails EEXIST →
    // no-replace preserved), then unlink completes the move. EXDEV cannot arise:
    // the staging temp and destination are always co-located on one filesystem.
    fs::hard_link(source, destination)?;
    if let Err(error) = fs::remove_file(source) {
        let _ = fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

fn run_daemon_client_command(
    endpoint: &str,
    json_output: bool,
    command: &DaemonClientCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = daemon_runtime().and_then(|runtime| {
        runtime.block_on(async {
            let channel = connect_daemon(endpoint)
                .await
                .map_err(DaemonClientError::from)?;
            let mut client = pb::daemon_client::DaemonClient::new(channel);
            match command {
                DaemonClientCommand::Health => {
                    let health = client.health(()).await.map_err(status_error)?.into_inner();
                    print_health(health, json_output, out).map_err(DaemonClientError::from)
                }
                DaemonClientCommand::Version => {
                    let version = client.version(()).await.map_err(status_error)?.into_inner();
                    print_version(version, json_output, out).map_err(DaemonClientError::from)
                }
            }
        })
    });
    finish_daemon_client_result(result, json_output, err)
}

fn run_operation_client_command(
    endpoint: &str,
    json_output: bool,
    command: &OperationClientCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = daemon_runtime().and_then(|runtime| {
        runtime.block_on(async {
            let channel = connect_daemon(endpoint)
                .await
                .map_err(DaemonClientError::from)?;
            let mut client = pb::daemon_client::DaemonClient::new(channel);
            match command {
                OperationClientCommand::Get { operation_id } => {
                    let operation_id = parse_uuid_bytes(operation_id, "operation_id")
                        .map_err(DaemonClientError::from)?;
                    let status = client
                        .get_operation(pb::GetOperationRequest { operation_id })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner();
                    print_operation(status, json_output, out).map_err(DaemonClientError::from)
                }
                OperationClientCommand::List => {
                    let operations = client
                        .list_operations(pb::ListOperationsRequest {
                            filter: Default::default(),
                            page_token: None,
                            page_size: 0,
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner()
                        .operations;
                    print_operation_list(operations, json_output, out)
                        .map_err(DaemonClientError::from)
                }
            }
        })
    });
    finish_daemon_client_result(result, json_output, err)
}

fn run_catalog_client_command(
    endpoint: &str,
    json_output: bool,
    command: &CatalogClientCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = daemon_runtime().and_then(|runtime| {
        runtime.block_on(async {
            let channel = connect_daemon(endpoint)
                .await
                .map_err(DaemonClientError::from)?;
            let mut client = pb::catalog_client::CatalogClient::new(channel);
            match command {
                CatalogClientCommand::Tapes { pool } => {
                    let tapes = client
                        .list_tapes(pb::ListTapesRequest {
                            library_uuid: Vec::new(),
                            page_token: None,
                            page_size: 0,
                            pool_id: pool.clone().unwrap_or_default(),
                            kind: "data".to_string(),
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner()
                        .tapes;
                    print_tape_list(tapes, json_output, out).map_err(DaemonClientError::from)
                }
                CatalogClientCommand::Tape { tape_uuid } => {
                    let tape_uuid = resolve_tape_uuid_arg(&mut client, tape_uuid).await?;
                    let tape = client
                        .get_tape(pb::GetTapeRequest { tape_uuid })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner();
                    print_tape(tape, json_output, out).map_err(DaemonClientError::from)
                }
                CatalogClientCommand::TapeFiles { tape_uuid } => {
                    let tape_uuid = parse_uuid_bytes(tape_uuid, "tape_uuid")
                        .map_err(DaemonClientError::from)?;
                    let tape_files = client
                        .list_tape_files(pb::ListTapeFilesRequest {
                            tape_uuid,
                            page_token: None,
                            page_size: 0,
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner()
                        .tape_files;
                    print_tape_file_list(tape_files, json_output, out)
                        .map_err(DaemonClientError::from)
                }
                CatalogClientCommand::Pools => {
                    let pools = client
                        .list_tape_pools(pb::ListTapePoolsRequest {
                            page_token: None,
                            page_size: 0,
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner()
                        .pools;
                    print_tape_pool_list(pools, json_output, out).map_err(DaemonClientError::from)
                }
                CatalogClientCommand::Pool { pool_id } => {
                    let pool = client
                        .get_tape_pool(pb::GetTapePoolRequest {
                            pool_id: pool_id.clone(),
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner();
                    print_tape_pool(pool, json_output, out).map_err(DaemonClientError::from)
                }
                CatalogClientCommand::Units { origin } => {
                    let mut stream = client
                        .enumerate_units(pb::EnumerateUnitsRequest {
                            scope: Some(pb::enumerate_units_request::Scope::All(())),
                            origin_filter: origin.to_proto() as i32,
                            refresh_from_source: false,
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner();
                    let mut units = Vec::new();
                    while let Some(unit) = stream.message().await.map_err(status_error)? {
                        units.push(unit);
                    }
                    print_catalog_unit_list(units, json_output, out)
                        .map_err(DaemonClientError::from)
                }
                CatalogClientCommand::Unit { unit_id } => {
                    let unit = client
                        .get_catalog_unit(pb::GetCatalogUnitRequest {
                            unit_id: unit_id.as_bytes().to_vec(),
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner();
                    print_catalog_unit(unit, json_output, out).map_err(DaemonClientError::from)
                }
                CatalogClientCommand::Entries { unit_id } => {
                    let response = client
                        .list_entries_in_unit(pb::ListEntriesInUnitRequest {
                            unit_id: unit_id.as_bytes().to_vec(),
                            page_token: None,
                            page_size: 0,
                            refresh_from_source: false,
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner();
                    print_catalog_entry_list(response.entries, json_output, out)
                        .map_err(DaemonClientError::from)
                }
            }
        })
    });
    finish_daemon_client_result(result, json_output, err)
}

fn run_audit_client_command(
    endpoint: &str,
    json_output: bool,
    command: &AuditClientCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = daemon_runtime().and_then(|runtime| {
        runtime.block_on(async {
            let channel = connect_daemon(endpoint)
                .await
                .map_err(DaemonClientError::from)?;
            let mut client = pb::audit_client::AuditClient::new(channel);
            match command {
                AuditClientCommand::Query {
                    since,
                    until,
                    filters,
                } => {
                    let since = parse_audit_timestamp(since, "since")?;
                    let until = parse_audit_timestamp(until, "until")?;
                    let filter = parse_audit_filters(filters)?;
                    let mut stream = client
                        .query_audit(pb::QueryAuditRequest {
                            since: Some(since),
                            until: Some(until),
                            filter: filter.into_iter().collect(),
                        })
                        .await
                        .map_err(status_error)?
                        .into_inner();
                    while let Some(entry) = stream.message().await.map_err(status_error)? {
                        print_audit_entry(entry, json_output, out)
                            .map_err(DaemonClientError::from)?;
                    }
                    Ok(())
                }
            }
        })
    });
    finish_daemon_client_result(result, json_output, err)
}

fn parse_audit_timestamp(
    value: &str,
    field: &str,
) -> Result<prost_types::Timestamp, DaemonClientError> {
    let parsed = OffsetDateTime::parse(value, &Rfc3339).map_err(|error| {
        DaemonClientError::client(format!(
            "invalid --{field} RFC3339 timestamp {value:?}: {error}"
        ))
    })?;
    Ok(prost_types::Timestamp {
        seconds: parsed.unix_timestamp(),
        nanos: i32::try_from(parsed.nanosecond()).expect("nanoseconds fit i32"),
    })
}

fn parse_audit_filters(filters: &[String]) -> Result<BTreeMap<String, String>, DaemonClientError> {
    let mut parsed = BTreeMap::new();
    for filter in filters {
        let (key, value) = filter.split_once('=').ok_or_else(|| {
            DaemonClientError::client(format!("invalid --filter {filter:?}; expected k=v"))
        })?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            return Err(DaemonClientError::client(format!(
                "invalid --filter {filter:?}; key and value must be nonempty"
            )));
        }
        if parsed.insert(key.to_string(), value.to_string()).is_some() {
            return Err(DaemonClientError::client(format!(
                "duplicate audit filter key {key:?}"
            )));
        }
    }
    Ok(parsed)
}

fn print_audit_entry(
    entry: pb::AuditEntry,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    let detail = serde_json::from_str::<Value>(&entry.detail_json)
        .unwrap_or_else(|_| Value::String(entry.detail_json.clone()));
    let value = json!({
        "sequence": entry.sequence,
        "timestamp": timestamp_value(entry.timestamp.as_ref()),
        "actor": entry.actor.clone(),
        "source_layer": entry.source_layer.clone(),
        "operation_id": optional_uuid_text(&entry.operation_id),
        "session_id": optional_uuid_text(&entry.session_id),
        "event_kind": entry.event_kind.clone(),
        "detail": detail,
    });
    if json_output {
        serde_json::to_writer(&mut *out, &value).map_err(|error| error.to_string())?;
        writeln!(out).map_err(|error| error.to_string())
    } else {
        writeln!(
            out,
            "{} {} {} {}",
            entry.sequence,
            timestamp_text(entry.timestamp.as_ref()).unwrap_or_else(|| "-".to_string()),
            entry.event_kind,
            entry.actor,
        )
        .map_err(|error| error.to_string())
    }
}

fn optional_uuid_text(bytes: &[u8]) -> Option<String> {
    (!bytes.is_empty()).then(|| bytes_to_uuid_text(bytes))
}

async fn resolve_tape_uuid_arg(
    client: &mut pb::catalog_client::CatalogClient<Channel>,
    arg: &str,
) -> Result<Vec<u8>, DaemonClientError> {
    if let Ok(uuid) = parse_uuid_bytes(arg, "tape_uuid") {
        return Ok(uuid);
    }
    let tapes = client
        .list_tapes(pb::ListTapesRequest {
            library_uuid: Vec::new(),
            page_token: None,
            page_size: 0,
            pool_id: String::new(),
            kind: "data".to_string(),
        })
        .await
        .map_err(status_error)?
        .into_inner()
        .tapes;
    tapes
        .into_iter()
        .find(|tape| tape.voltag == arg)
        .map(|tape| tape.tape_uuid)
        .ok_or_else(|| {
            DaemonClientError::client(format!("tape {arg:?} not found by UUID or voltag"))
        })
}

async fn resolve_drive_uuid_arg(
    client: &mut pb::library_service_client::LibraryServiceClient<Channel>,
    arg: &str,
) -> Result<Vec<u8>, DaemonClientError> {
    if let Ok(uuid) = parse_uuid_bytes(arg, "drive_uuid") {
        return Ok(uuid);
    }
    let drive = client
        .get_drive(pb::GetDriveRequest {
            drive: arg.to_string(),
        })
        .await
        .map_err(drive_status_error)?
        .into_inner();
    drive_uuid_from_selector(arg, Some(drive)).map_err(DaemonClientError::client)
}

async fn resolve_library_arg(
    client: &mut pb::library_service_client::LibraryServiceClient<Channel>,
    arg: &str,
) -> Result<pb::Library, DaemonClientError> {
    let libraries = client
        .list_libraries(())
        .await
        .map_err(status_error)?
        .into_inner()
        .libraries;
    let requested_uuid = Uuid::parse_str(arg).ok();
    libraries
        .into_iter()
        .find(|library| {
            library.library_serial == arg
                || requested_uuid.is_some_and(|requested| {
                    library.library_uuid.as_slice() == requested.as_bytes()
                })
        })
        .ok_or_else(|| {
            DaemonClientError::client(format!("library {arg:?} was not found by serial or UUID"))
        })
}

async fn wait_for_operation(
    channel: Channel,
    operation_id: Vec<u8>,
) -> Result<pb::OperationStatus, DaemonClientError> {
    let mut stream = pb::daemon_client::DaemonClient::new(channel)
        .watch_operation(pb::GetOperationRequest { operation_id })
        .await
        .map_err(status_error)?
        .into_inner();
    let mut terminal = None;
    while let Some(status) = stream.message().await.map_err(status_error)? {
        let state =
            pb::OperationState::try_from(status.state).unwrap_or(pb::OperationState::Unspecified);
        if matches!(
            state,
            pb::OperationState::Succeeded
                | pb::OperationState::Failed
                | pb::OperationState::Cancelled
                | pb::OperationState::Unknown
        ) {
            terminal = Some(status);
            break;
        }
    }
    let status = terminal.ok_or_else(|| {
        DaemonClientError::client("operation watch ended before a terminal status")
    })?;
    if status.state != pb::OperationState::Succeeded as i32 {
        let state = operation_state_name(status.state);
        let detail = if status.error_summary.is_empty() {
            state.to_string()
        } else {
            format!("{state}: {}", status.error_summary)
        };
        return Err(DaemonClientError::client(format!(
            "operation {} finished {detail}",
            bytes_to_uuid_text(&status.operation_id)
        )));
    }
    Ok(status)
}

fn drive_uuid_from_selector(
    selector: &str,
    drive: Option<pb::DriveCatalogEntry>,
) -> Result<Vec<u8>, String> {
    if let Ok(uuid) = parse_uuid_bytes(selector, "drive_uuid") {
        return Ok(uuid);
    }
    let drive = drive.ok_or_else(|| {
        format!("drive {selector:?} was not found by UUID or device-reported serial")
    })?;
    if !drive.actionable {
        return Err(format!(
            "drive serial {selector:?} is ambiguous or non-actionable; resolve the collision and retry with a drive UUID"
        ));
    }
    if drive.drive_uuid.is_empty() {
        return Err(format!(
            "drive serial {selector:?} resolved without a drive UUID"
        ));
    }
    Ok(drive.drive_uuid)
}

fn poll_drive_request(drive: &str) -> pb::PollDriveRequest {
    pb::PollDriveRequest {
        drive: drive.to_string(),
        allow_derived_identity: false,
    }
}

fn run_drive_client_command(
    endpoint: &str,
    json_output: bool,
    command: &DriveClientCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = daemon_runtime().and_then(|runtime| {
        runtime.block_on(async {
            let channel = connect_daemon(endpoint)
                .await
                .map_err(DaemonClientError::from)?;
            let mut client =
                pb::library_service_client::LibraryServiceClient::new(channel.clone());
            match command {
                DriveClientCommand::Load(args) => {
                    if args.barcode.is_some() == args.slot.is_some() {
                        return Err(DaemonClientError::client(
                            "drive load requires exactly one of --barcode or --slot",
                        ));
                    }
                    let library = resolve_library_arg(&mut client, args.library.as_str()).await?;
                    let slot = if let Some(slot) = args.slot {
                        u32::from(slot)
                    } else {
                        let barcode = args.barcode.as_deref().expect("clap requires load source");
                        let state = client
                            .get_library(pb::GetLibraryRequest {
                                library_uuid: library.library_uuid.clone(),
                            })
                            .await
                            .map_err(status_error)?
                            .into_inner();
                        state
                            .slots
                            .iter()
                            .find(|slot| slot.voltag == barcode)
                            .map(|slot| slot.element_address)
                            .ok_or_else(|| {
                                DaemonClientError::client(format!(
                                    "barcode {barcode:?} is not present in a storage slot of library {:?}",
                                    library.library_serial
                                ))
                            })?
                    };
                    let operation = client
                        .load_drive(pb::LoadDriveRequest {
                            library_uuid: library.library_uuid,
                            slot_element_address: slot,
                            drive_element_address: u32::from(args.bay),
                            idempotency_key: None,
                            no_wait: args.no_wait,
                        })
                        .await
                        .map_err(status_error)?
                        .into_inner();
                    let status = wait_for_operation(channel, operation.operation_id).await?;
                    print_operation(status, json_output, out).map_err(DaemonClientError::from)
                }
                DriveClientCommand::List { foreign, retired } => {
                    let drives = client
                        .list_drives(pb::ListDrivesRequest {
                            include_foreign: *foreign,
                            include_retired: *retired,
                            page_token: None,
                            page_size: 0,
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner()
                        .drives;
                    print_drive_list(drives, json_output, out).map_err(DaemonClientError::from)
                }
                DriveClientCommand::Show { drive } => {
                    let drive = client
                        .get_drive(pb::GetDriveRequest {
                            drive: drive.clone(),
                        })
                        .await
                        .map_err(drive_status_error)?
                        .into_inner();
                    print_drive(drive, json_output, out).map_err(DaemonClientError::from)
                }
                DriveClientCommand::History {
                    drive,
                    events,
                    snapshots,
                } => {
                    let history = client
                        .get_drive_history(pb::GetDriveHistoryRequest {
                            drive: drive.clone(),
                            include_events: *events,
                            include_snapshots: *snapshots,
                            page_token: None,
                            page_size: 0,
                        })
                        .await
                        .map_err(status_error)?
                        .into_inner();
                    print_drive_history(history, json_output, out).map_err(DaemonClientError::from)
                }
                DriveClientCommand::Alerts { drive } => {
                    let snapshot = client
                        .poll_drive(poll_drive_request(drive))
                        .await
                        .map_err(drive_status_error)?
                        .into_inner();
                    print_drive_snapshot(snapshot, json_output, out)
                        .map_err(DaemonClientError::from)
                }
                DriveClientCommand::Annotate(args) => {
                    let drive_uuid = resolve_drive_uuid_arg(&mut client, &args.drive).await?;
                    let drive = client
                        .annotate_drive(pb::AnnotateDriveRequest {
                            drive_uuid,
                            purchase_date: args.purchase_date.clone().unwrap_or_default(),
                            warranty_until: args.warranty_until.clone().unwrap_or_default(),
                            cost: args.cost.clone().unwrap_or_default(),
                            note: args.note.clone().unwrap_or_default(),
                            notes_set: args.notes_set.clone().unwrap_or_default(),
                            allow_derived_identity: args.allow_derived_identity,
                        })
                        .await
                        .map_err(status_error)?
                        .into_inner();
                    print_drive(drive, json_output, out).map_err(DaemonClientError::from)
                }
                DriveClientCommand::Retire(args) => {
                    let drive_uuid = resolve_drive_uuid_arg(&mut client, &args.drive).await?;
                    let response = client
                        .retire_drive(pb::RetireDriveRequest {
                            drive_uuid,
                            reason: args.reason.clone(),
                            i_understand_fleet_removal_is_permanent: args
                                .i_understand_fleet_removal_is_permanent,
                            allow_derived_identity: args.allow_derived_identity,
                        })
                        .await
                        .map_err(status_error)?
                        .into_inner();
                    print_drive_retire(response, json_output, out).map_err(DaemonClientError::from)
                }
                DriveClientCommand::Poll { drive } => {
                    let snapshot = client
                        .poll_drive(poll_drive_request(drive))
                        .await
                        .map_err(drive_status_error)?
                        .into_inner();
                    print_drive_snapshot(snapshot, json_output, out)
                        .map_err(DaemonClientError::from)
                }
                DriveClientCommand::Clean { drive } => {
                    let drive_uuid = resolve_drive_uuid_arg(&mut client, drive).await?;
                    let operation = client
                        .clean_drive(pb::CleanDriveRequest {
                            drive_uuid,
                            allow_derived_identity: false,
                            idempotency_key: None,
                        })
                        .await
                        .map_err(status_error)?
                        .into_inner();
                    print_operation_ref(operation, json_output, out)
                        .map_err(DaemonClientError::from)
                }
            }
        })
    });
    finish_daemon_client_result(result, json_output, err)
}

fn run_archive_verify_client(
    endpoint: &str,
    library: &str,
    drive: u16,
    locator: &str,
    expected_sha256: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = daemon_runtime().and_then(|runtime| {
        runtime.block_on(async {
            let locator = pool_ops::decode_locator(locator)
                .map_err(|error| DaemonClientError::client(format!("locator: {error}")))?;
            let expected: [u8; 32] = pool_ops::hex_to_bytes(expected_sha256)
                .and_then(|bytes| {
                    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| {
                        format!("expected-sha256 must be 32 bytes, got {}", bytes.len())
                    })
                })
                .map_err(|error| {
                    DaemonClientError::client(format!("--expected-sha256: {error}"))
                })?;
            let object_id = Uuid::parse_str(locator.object_id.as_str()).map_err(|error| {
                DaemonClientError::client(format!("locator object_id: {error}"))
            })?;
            let channel = connect_daemon(endpoint)
                .await
                .map_err(DaemonClientError::from)?;
            let library = {
                let mut client =
                    pb::library_service_client::LibraryServiceClient::new(channel.clone());
                resolve_library_arg(&mut client, library).await?
            };
            let mut client =
                pb::read_session_service_client::ReadSessionServiceClient::new(channel);
            let session = client
                .open_read_session(pb::OpenReadSessionRequest {
                    target: Some(pb::open_read_session_request::Target::DriveTarget(
                        pb::DriveTarget {
                            library_uuid: library.library_uuid,
                            drive_element_address: u32::from(drive),
                            required_pool_id: String::new(),
                        },
                    )),
                    idempotency_key: None,
                    resume_target: None,
                })
                .await
                .map_err(status_error)?
                .into_inner();
            if session.tape_uuid.as_slice() != locator.tape_uuid {
                let original = DaemonClientError::client(format!(
                    "drive 0x{drive:04x} contains tape {}, but locator requires {}",
                    bytes_to_uuid_text(&session.tape_uuid),
                    Uuid::from_bytes(locator.tape_uuid),
                ));
                let _ = client
                    .close_read_session(pb::CloseReadSessionRequest {
                        session_id: session.session_id,
                        idempotency_key: None,
                    })
                    .await;
                return Err(original);
            }
            let session_id = session.session_id;
            let read_result = async {
                let mut stream = client
                    .read_file(pb::ReadFileRequest {
                        session_id: session_id.clone(),
                        object_id: object_id.as_bytes().to_vec(),
                        file_id: Vec::new(),
                        stream_chunk_bytes: 0,
                    })
                    .await
                    .map_err(status_error)?
                    .into_inner();
                let mut hasher = Sha256::new();
                let mut bytes_read = 0u64;
                let mut saw_terminal = false;
                while let Some(chunk) = stream.message().await.map_err(status_error)? {
                    hasher.update(&chunk.data);
                    bytes_read = bytes_read
                        .checked_add(chunk.data.len() as u64)
                        .ok_or_else(|| DaemonClientError::client("verified byte count overflow"))?;
                    saw_terminal |= chunk.is_last;
                }
                if !saw_terminal {
                    return Err(DaemonClientError::client(
                        "read stream ended without a terminal chunk",
                    ));
                }
                Ok::<_, DaemonClientError>((bytes_read, <[u8; 32]>::from(hasher.finalize())))
            }
            .await;
            let close_result = client
                .close_read_session(pb::CloseReadSessionRequest {
                    session_id,
                    idempotency_key: None,
                })
                .await
                .map_err(status_error);
            let (bytes_read, actual) = match read_result {
                Err(original) => return Err(original),
                Ok(outcome) => {
                    close_result?;
                    outcome
                }
            };
            let verified = actual == expected;
            let receipt = json!({
                "verified": verified,
                "expected_sha256": bytes_to_hex(&expected),
                "actual_sha256": bytes_to_hex(&actual),
                "bytes_read": bytes_read,
                "tape_uuid": Uuid::from_bytes(locator.tape_uuid).to_string(),
                "drive_element_address": drive,
            });
            serde_json::to_writer(&mut *out, &receipt)
                .map_err(|error| DaemonClientError::client(error.to_string()))?;
            writeln!(out).map_err(|error| DaemonClientError::client(error.to_string()))?;
            if !verified {
                return Err(DaemonClientError::client(
                    "sha256 mismatch (drive payload vs --expected-sha256)",
                ));
            }
            Ok(())
        })
    });
    finish_daemon_client_result(result, false, err)
}

fn run_alarms_client_command(
    endpoint: &str,
    json_output: bool,
    all: bool,
    command: &Option<AlarmsClientCommand>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = daemon_runtime().and_then(|runtime| {
        runtime.block_on(async {
            let channel = connect_daemon(endpoint)
                .await
                .map_err(DaemonClientError::from)?;
            let mut client = pb::library_service_client::LibraryServiceClient::new(channel);
            match command {
                None => {
                    let alarms = client
                        .list_alarms(pb::ListAlarmsRequest {
                            include_cleared: all,
                            page_token: None,
                            page_size: 0,
                        })
                        .await
                        .map_err(status_error)?
                        .into_inner()
                        .alarms;
                    print_alarm_list(alarms, json_output, out).map_err(DaemonClientError::from)
                }
                Some(AlarmsClientCommand::Ack { condition_key }) => {
                    let alarm = client
                        .ack_alarm(pb::AckAlarmRequest {
                            condition_key: condition_key.clone(),
                            idempotency_key: None,
                        })
                        .await
                        .map_err(status_error)?
                        .into_inner();
                    print_alarm(alarm, json_output, out).map_err(DaemonClientError::from)
                }
            }
        })
    });
    finish_daemon_client_result(result, json_output, err)
}

fn run_top_command(
    endpoint: &str,
    json_output: bool,
    once: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    if !json_output && !once {
        #[cfg(feature = "tui")]
        {
            return top::run_top_tui(endpoint, out, err);
        }
        #[cfg(not(feature = "tui"))]
        {
            let _ = writeln!(
                err,
                "error: `rem top` interactive mode requires the `tui` feature"
            );
            let _ = writeln!(
                err,
                "       rebuild with default features or use `rem top --once --json`"
            );
            return ExitCode::from(1);
        }
    }

    let result = daemon_runtime().and_then(|runtime| {
        runtime.block_on(async {
            let channel = connect_daemon(endpoint)
                .await
                .map_err(DaemonClientError::from)?;
            let mut client = pb::library_service_client::LibraryServiceClient::new(channel);
            let response = client
                .get_live_status(pb::GetLiveStatusRequest {})
                .await
                .map_err(drive_status_error)?
                .into_inner();
            if json_output {
                print_live_status_json(&response, out).map_err(DaemonClientError::from)
            } else {
                print_live_status_text(&response, out).map_err(DaemonClientError::from)
            }
        })
    });
    finish_top_client_result(result, json_output, err)
}

impl CatalogUnitOriginFilterArg {
    fn to_proto(self) -> pb::CatalogUnitOriginFilter {
        match self {
            Self::All => pb::CatalogUnitOriginFilter::Unspecified,
            Self::Native => pb::CatalogUnitOriginFilter::NativeObjects,
            Self::Foreign => pb::CatalogUnitOriginFilter::ForeignArchives,
        }
    }
}

#[derive(Debug)]
struct DaemonClientError {
    code: &'static str,
    message: String,
}

impl DaemonClientError {
    fn client(message: impl Into<String>) -> Self {
        Self {
            code: "daemon_client_error",
            message: message.into(),
        }
    }

    fn status(error: tonic::Status) -> Self {
        let code = tonic_code_name(error.code());
        Self {
            code,
            message: format!("daemon returned {code}: {}", error.message()),
        }
    }
}

impl From<String> for DaemonClientError {
    fn from(message: String) -> Self {
        Self::client(message)
    }
}

fn daemon_runtime() -> Result<tokio::runtime::Runtime, DaemonClientError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| DaemonClientError::client(format!("create tokio runtime: {error}")))
}

async fn connect_daemon(endpoint: &str) -> Result<Channel, String> {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        return remanence_api::connect_unix(std::path::PathBuf::from(path))
            .await
            .map_err(|error| format!("connect daemon at {endpoint}: {error}"));
    }
    Channel::from_shared(endpoint.to_string())
        .map_err(|error| format!("invalid daemon endpoint {endpoint:?}: {error}"))?
        .connect()
        .await
        .map_err(|error| format!("connect daemon at {endpoint}: {error}"))
}

fn finish_daemon_client_result(
    result: Result<(), DaemonClientError>,
    json_output: bool,
    err: &mut dyn Write,
) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if json_output {
                let _ = print_json_error(error.code, &error.message, err);
            } else {
                let _ = writeln!(err, "error: {}", error.message);
            }
            ExitCode::from(1)
        }
    }
}

fn finish_top_client_result(
    result: Result<(), DaemonClientError>,
    json_output: bool,
    err: &mut dyn Write,
) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if json_output {
                let _ = print_json_error(error.code, &error.message, err);
            } else {
                let _ = writeln!(err, "error: {}", error.message);
            }
            if error.message.starts_with("connect daemon at ") {
                let _ = writeln!(
                    err,
                    "       use `rem library <serial> --slots` to inspect the current library state"
                );
            }
            ExitCode::from(1)
        }
    }
}

fn status_error(error: tonic::Status) -> DaemonClientError {
    DaemonClientError::status(error)
}

fn drive_status_error(error: tonic::Status) -> DaemonClientError {
    if error.code() == tonic::Code::Unimplemented {
        return DaemonClientError::client("daemon predates drive stewardship; upgrade rem-daemon");
    }
    DaemonClientError::status(error)
}

fn tonic_code_name(code: tonic::Code) -> &'static str {
    match code {
        tonic::Code::Ok => "ok",
        tonic::Code::Cancelled => "cancelled",
        tonic::Code::Unknown => "unknown",
        tonic::Code::InvalidArgument => "invalid_argument",
        tonic::Code::DeadlineExceeded => "deadline_exceeded",
        tonic::Code::NotFound => "not_found",
        tonic::Code::AlreadyExists => "already_exists",
        tonic::Code::PermissionDenied => "permission_denied",
        tonic::Code::ResourceExhausted => "resource_exhausted",
        tonic::Code::FailedPrecondition => "failed_precondition",
        tonic::Code::Aborted => "aborted",
        tonic::Code::OutOfRange => "out_of_range",
        tonic::Code::Unimplemented => "unimplemented",
        tonic::Code::Internal => "internal",
        tonic::Code::Unavailable => "unavailable",
        tonic::Code::DataLoss => "data_loss",
        tonic::Code::Unauthenticated => "unauthenticated",
    }
}

fn parse_uuid_bytes(value: &str, field: &str) -> Result<Vec<u8>, String> {
    Uuid::parse_str(value)
        .map(|uuid| uuid.as_bytes().to_vec())
        .map_err(|error| format!("invalid {field} {value:?}: {error}"))
}

fn print_health(
    health: pb::HealthResponse,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope("rem.daemon.health.v1", "item", health_json(&health), out);
    }
    let _ = writeln!(out, "status: {}", health_status_name(health.status));
    if !health.detail.is_empty() {
        let _ = writeln!(out, "detail: {}", health.detail);
    }
    let mut components = health.components.into_iter().collect::<Vec<_>>();
    components.sort_by(|a, b| a.0.cmp(&b.0));
    if !components.is_empty() {
        let _ = writeln!(out, "components:");
        for (name, status) in components {
            let _ = writeln!(out, "  {name}: {status}");
        }
    }
    Ok(())
}

fn print_version(
    version: pb::VersionResponse,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope("rem.daemon.version.v1", "item", version_json(&version), out);
    }
    let _ = writeln!(out, "daemon: {}", version.daemon_version);
    let _ = writeln!(out, "api: {}", version.api_version);
    let _ = writeln!(out, "target: {}", version.rust_target);
    Ok(())
}

fn print_operation(
    operation: pb::OperationStatus,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.operation.status.v1",
            "item",
            operation_json(&operation),
            out,
        );
    }
    print_operation_line(&operation, out);
    if !operation.progress.is_empty() {
        let _ = writeln!(out, "  progress:");
        let mut progress = operation.progress.into_iter().collect::<Vec<_>>();
        progress.sort_by(|a, b| a.0.cmp(&b.0));
        for (key, value) in progress {
            let _ = writeln!(out, "    {key}: {value}");
        }
    }
    if !operation.error_summary.is_empty() {
        let _ = writeln!(out, "  error: {}", operation.error_summary);
    }
    Ok(())
}

fn print_operation_ref(
    operation: pb::OperationRef,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        let json = json!({
            "operation_id": bytes_to_uuid_text(&operation.operation_id),
        });
        serde_json::to_writer(&mut *out, &json).map_err(|err| err.to_string())?;
        writeln!(out).map_err(|err| err.to_string())?;
        return Ok(());
    }
    writeln!(
        out,
        "operation {}",
        bytes_to_uuid_text(&operation.operation_id)
    )
    .map_err(|err| err.to_string())
}

fn print_operation_list(
    operations: Vec<pb::OperationStatus>,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.operation.status.list.v1",
            "list",
            json!({ "operations": operations.iter().map(operation_json).collect::<Vec<_>>() }),
            out,
        );
    }
    if operations.is_empty() {
        let _ = writeln!(out, "(no operations)");
    } else {
        for operation in operations {
            print_operation_line(&operation, out);
        }
    }
    Ok(())
}

fn print_operation_line(operation: &pb::OperationStatus, out: &mut dyn Write) {
    let operation_id = bytes_to_uuid_text(&operation.operation_id);
    let state = operation_state_name(operation.state);
    let updated = timestamp_text(operation.updated_at.as_ref()).unwrap_or_else(|| "-".into());
    let _ = writeln!(
        out,
        "{operation_id}  {state}  kind={}  updated={updated}",
        operation.operation_kind
    );
}

fn print_tape(tape: pb::Tape, json_output: bool, out: &mut dyn Write) -> Result<(), String> {
    if json_output {
        return print_json_envelope("rem.catalog.tape.v1", "item", tape_json(&tape), out);
    }
    print_tape_line(&tape, out);
    for rollup in &tape.correlation_rollups {
        print_rollup_line("  drive", rollup, out);
    }
    Ok(())
}

fn print_tape_list(
    tapes: Vec<pb::Tape>,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.catalog.tape.list.v1",
            "list",
            json!({ "tapes": tapes.iter().map(tape_json).collect::<Vec<_>>() }),
            out,
        );
    }
    if tapes.is_empty() {
        let _ = writeln!(out, "(no tapes)");
    } else {
        for tape in tapes {
            print_tape_line(&tape, out);
        }
    }
    Ok(())
}

fn print_tape_line(tape: &pb::Tape, out: &mut dyn Write) {
    let tape_uuid = bytes_to_uuid_text(&tape.tape_uuid);
    let voltag = dash_if_empty(&tape.voltag);
    let pool = dash_if_empty(&tape.pool_id);
    let _ = writeln!(
        out,
        "{tape_uuid}  {voltag}  {}  state={}  pool={pool}  last_file={}",
        tape.body_format,
        tape_state_name(tape.state),
        tape.last_committed_tape_file
    );
}

fn print_drive(
    drive: pb::DriveCatalogEntry,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope("rem.drive.show.v1", "item", drive_json(&drive), out);
    }
    print_drive_line(&drive, out);
    Ok(())
}

fn print_drive_list(
    drives: Vec<pb::DriveCatalogEntry>,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.drive.list.v1",
            "list",
            json!({ "drives": drives.iter().map(drive_json).collect::<Vec<_>>() }),
            out,
        );
    }
    if drives.is_empty() {
        let _ = writeln!(out, "(no drives)");
    } else {
        for drive in drives {
            print_drive_line(&drive, out);
        }
    }
    Ok(())
}

fn print_drive_history(
    history: pb::GetDriveHistoryResponse,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.drive.history.v1",
            "item",
            json!({
                "drive": history.drive.as_ref().map(drive_json),
                "events": history.events.iter().map(drive_event_json).collect::<Vec<_>>(),
                "snapshots": history.snapshots.iter().map(drive_snapshot_json).collect::<Vec<_>>()
            }),
            out,
        );
    }
    if let Some(drive) = history.drive.as_ref() {
        print_drive_line(drive, out);
    }
    for event in history.events {
        let at = timestamp_text(event.at_utc.as_ref()).unwrap_or_else(|| "-".into());
        let _ = writeln!(out, "  event {at} {}", event.event_kind);
    }
    for snapshot in history.snapshots {
        let at = timestamp_text(snapshot.at_utc.as_ref()).unwrap_or_else(|| "-".into());
        let _ = writeln!(out, "  snapshot {at} trigger={}", snapshot.trigger);
    }
    Ok(())
}

fn print_drive_snapshot(
    snapshot: pb::DriveHealthSnapshot,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.drive.snapshot.v1",
            "item",
            drive_snapshot_json(&snapshot),
            out,
        );
    }
    let at = timestamp_text(snapshot.at_utc.as_ref()).unwrap_or_else(|| "-".into());
    let _ = writeln!(
        out,
        "snapshot {}  trigger={}  flags={}  write_uncorrected={}  read_uncorrected={}",
        at,
        snapshot.trigger,
        snapshot.tape_alert_flags,
        snapshot.write_errors_uncorrected,
        snapshot.read_errors_uncorrected
    );
    Ok(())
}

fn print_drive_retire(
    response: pb::RetireDriveResponse,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.drive.retire.v1",
            "item",
            json!({
                "drive": response.drive.as_ref().map(drive_json),
                "newly_retired": response.newly_retired
            }),
            out,
        );
    }
    if let Some(drive) = response.drive.as_ref() {
        print_drive_line(drive, out);
    }
    let _ = writeln!(out, "newly_retired: {}", response.newly_retired);
    Ok(())
}

fn print_alarm(alarm: pb::Alarm, json_output: bool, out: &mut dyn Write) -> Result<(), String> {
    if json_output {
        return print_json_envelope("rem.alarms.v1", "item", alarm_json(&alarm), out);
    }
    print_alarm_line(&alarm, out);
    Ok(())
}

fn print_alarm_list(
    alarms: Vec<pb::Alarm>,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.alarms.v1",
            "list",
            json!({ "alarms": alarms.iter().map(alarm_json).collect::<Vec<_>>() }),
            out,
        );
    }
    if alarms.is_empty() {
        let _ = writeln!(out, "(no alarms)");
    } else {
        for alarm in alarms {
            print_alarm_line(&alarm, out);
        }
    }
    Ok(())
}

fn print_drive_line(drive: &pb::DriveCatalogEntry, out: &mut dyn Write) {
    let uuid = bytes_to_uuid_text(&drive.drive_uuid);
    let serial = if drive.serial.is_empty() {
        "unattributed (pre-stewardship)"
    } else {
        drive.serial.as_str()
    };
    let label = if drive.managed == "foreign" {
        if drive.last_library_serial.is_empty() {
            format!("[foreign] {serial}")
        } else {
            format!("[foreign: {}] {serial}", drive.last_library_serial)
        }
    } else {
        serial.to_string()
    };
    let _ = writeln!(
        out,
        "{uuid}  {label}  state={}  cleaning_due={}  fenced={}",
        drive.state, drive.cleaning_due, drive.fenced
    );
    for rollup in &drive.correlation_rollups {
        print_rollup_line("  tape", rollup, out);
    }
}

fn print_rollup_line(prefix: &str, rollup: &pb::DriveCorrelationRollup, out: &mut dyn Write) {
    let voltag = if rollup.voltag.is_empty() {
        "(unknown-voltag)"
    } else {
        rollup.voltag.as_str()
    };
    let drive = if rollup.drive_serial.is_empty() {
        "unattributed (pre-stewardship)"
    } else {
        rollup.drive_serial.as_str()
    };
    let _ = writeln!(
        out,
        "{prefix} {voltag}  drive={drive}  sessions={}  snapshots={}  write_uncorrected={}  read_uncorrected={}",
        rollup.session_count,
        rollup.snapshot_count,
        rollup.write_errors_uncorrected,
        rollup.read_errors_uncorrected
    );
}

fn print_alarm_line(alarm: &pb::Alarm, out: &mut dyn Write) {
    let last_seen = timestamp_text(alarm.last_seen_utc.as_ref()).unwrap_or_else(|| "-".into());
    let _ = writeln!(
        out,
        "{}  {}  severity={}  state={}  last_seen={last_seen}",
        alarm.condition_key, alarm.kind, alarm.severity, alarm.state
    );
}

fn print_tape_file_list(
    tape_files: Vec<pb::TapeFile>,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.catalog.tape_file.list.v1",
            "list",
            json!({ "tape_files": tape_files.iter().map(tape_file_json).collect::<Vec<_>>() }),
            out,
        );
    }
    if tape_files.is_empty() {
        let _ = writeln!(out, "(no tape files)");
    } else {
        for file in tape_files {
            let object_id = if file.object_id.is_empty() {
                "-".to_string()
            } else {
                bytes_to_uuid_text(&file.object_id)
            };
            let _ = writeln!(
                out,
                "file={}  kind={}  blocks={}  object={object_id}",
                file.tape_file_number, file.kind, file.block_count
            );
        }
    }
    Ok(())
}

fn print_tape_pool(
    pool: pb::TapePool,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.catalog.tape_pool.v1",
            "item",
            tape_pool_json(&pool),
            out,
        );
    }
    print_tape_pool_line(&pool, out);
    Ok(())
}

fn print_tape_pool_list(
    pools: Vec<pb::TapePool>,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.catalog.tape_pool.list.v1",
            "list",
            json!({ "pools": pools.iter().map(tape_pool_json).collect::<Vec<_>>() }),
            out,
        );
    }
    if pools.is_empty() {
        let _ = writeln!(out, "(no tape pools)");
    } else {
        for pool in pools {
            print_tape_pool_line(&pool, out);
        }
    }
    Ok(())
}

fn print_tape_pool_line(pool: &pb::TapePool, out: &mut dyn Write) {
    let label = dash_if_empty(&pool.display_name);
    let copy_class = dash_if_empty(&pool.copy_class);
    let content_class = dash_if_empty(&pool.content_class);
    let _ = writeln!(
        out,
        "{}  label={label}  copy={copy_class}  content={content_class}",
        pool.pool_id
    );
}

fn print_catalog_unit(
    unit: pb::CatalogUnit,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope("rem.catalog.unit.v1", "item", catalog_unit_json(&unit), out);
    }
    print_catalog_unit_line(&unit, out);
    Ok(())
}

fn print_catalog_unit_list(
    units: Vec<pb::CatalogUnit>,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.catalog.unit.list.v1",
            "list",
            json!({ "units": units.iter().map(catalog_unit_json).collect::<Vec<_>>() }),
            out,
        );
    }
    if units.is_empty() {
        let _ = writeln!(out, "(no catalog units)");
    } else {
        for unit in units {
            print_catalog_unit_line(&unit, out);
        }
    }
    Ok(())
}

fn print_catalog_unit_line(unit: &pb::CatalogUnit, out: &mut dyn Write) {
    let unit_id = bytes_to_text_or_uuid(&unit.unit_id);
    let tape_uuid = bytes_to_uuid_text(&unit.tape_uuid);
    let _ = writeln!(
        out,
        "{unit_id}  tape={tape_uuid}  format={}  origin={}",
        unit.format_id,
        catalog_unit_origin_name(unit.origin_kind)
    );
}

fn print_catalog_entry_list(
    entries: Vec<pb::CatalogEntry>,
    json_output: bool,
    out: &mut dyn Write,
) -> Result<(), String> {
    if json_output {
        return print_json_envelope(
            "rem.catalog.entry.list.v1",
            "list",
            json!({ "entries": entries.iter().map(catalog_entry_json).collect::<Vec<_>>() }),
            out,
        );
    }
    if entries.is_empty() {
        let _ = writeln!(out, "(no catalog entries)");
    } else {
        for entry in entries {
            let entry_id = bytes_to_text_or_uuid(&entry.entry_id);
            let size = entry
                .size_bytes
                .map(|size| size.to_string())
                .unwrap_or_else(|| "-".to_string());
            let _ = writeln!(
                out,
                "{entry_id}  {}  {}  size={size}  {}",
                catalog_entry_kind_name(entry.kind),
                catalog_entry_state_name(entry.state),
                entry.path
            );
        }
    }
    Ok(())
}

fn print_json_envelope(
    schema: &str,
    kind: &str,
    data: Value,
    out: &mut dyn Write,
) -> Result<(), String> {
    let envelope = json!({
        "schema": schema,
        "kind": kind,
        "data": data,
        "operation": null
    });
    serde_json::to_writer(&mut *out, &envelope).map_err(|error| error.to_string())?;
    writeln!(out).map_err(|error| error.to_string())
}

fn print_json_error(code: &str, message: &str, err: &mut dyn Write) -> Result<(), String> {
    let envelope = json!({
        "schema": "rem.error.v1",
        "kind": "error",
        "code": code,
        "message": message,
        "details": {}
    });
    serde_json::to_writer(&mut *err, &envelope).map_err(|error| error.to_string())?;
    writeln!(err).map_err(|error| error.to_string())
}

fn health_json(health: &pb::HealthResponse) -> Value {
    json!({
        "status": health_status_name(health.status),
        "components": health.components,
        "detail": health.detail,
    })
}

fn version_json(version: &pb::VersionResponse) -> Value {
    json!({
        "daemon_version": version.daemon_version,
        "api_version": version.api_version,
        "rust_target": version.rust_target,
    })
}

fn operation_json(operation: &pb::OperationStatus) -> Value {
    json!({
        "operation_id": bytes_to_uuid_text(&operation.operation_id),
        "operation_kind": operation.operation_kind,
        "state": operation_state_name(operation.state),
        "created_at": timestamp_value(operation.created_at.as_ref()),
        "updated_at": timestamp_value(operation.updated_at.as_ref()),
        "progress": operation.progress,
        "error_summary": operation.error_summary,
    })
}

fn tape_json(tape: &pb::Tape) -> Value {
    json!({
        "tape_uuid": bytes_to_uuid_text(&tape.tape_uuid),
        "voltag": tape.voltag,
        "body_format": tape.body_format,
        "block_size_bytes": tape.block_size_bytes,
        "data_blocks_per_stripe": tape.data_blocks_per_stripe,
        "parity_blocks_per_stripe": tape.parity_blocks_per_stripe,
        "stripes_per_neighborhood": tape.stripes_per_neighborhood,
        "last_committed_tape_file": tape.last_committed_tape_file,
        "state": tape_state_name(tape.state),
        "updated_at": timestamp_value(tape.updated_at.as_ref()),
        "pool_id": tape.pool_id,
        "correlation_rollups": tape.correlation_rollups.iter().map(correlation_rollup_json).collect::<Vec<_>>(),
    })
}

fn drive_json(drive: &pb::DriveCatalogEntry) -> Value {
    json!({
        "drive_uuid": bytes_to_uuid_text(&drive.drive_uuid),
        "serial": drive.serial,
        "identity_source": drive.identity_source,
        "actionable": drive.actionable,
        "vendor": drive.vendor,
        "product": drive.product,
        "firmware_rev": drive.firmware_rev,
        "managed": drive.managed,
        "state": drive.state,
        "cleaning_due": drive.cleaning_due,
        "fenced": drive.fenced,
        "first_seen_utc": timestamp_value(drive.first_seen_utc.as_ref()),
        "last_seen_utc": timestamp_value(drive.last_seen_utc.as_ref()),
        "last_library_serial": drive.last_library_serial,
        "last_element_address": drive.last_element_address,
        "purchase_date": drive.purchase_date,
        "warranty_until": drive.warranty_until,
        "cost": drive.cost,
        "notes": drive.notes,
        "retired_at_utc": timestamp_value(drive.retired_at_utc.as_ref()),
        "retire_reason": drive.retire_reason,
        "correlation_rollups": drive.correlation_rollups.iter().map(correlation_rollup_json).collect::<Vec<_>>(),
    })
}

fn correlation_rollup_json(rollup: &pb::DriveCorrelationRollup) -> Value {
    json!({
        "tape_uuid": bytes_to_uuid_text(&rollup.tape_uuid),
        "voltag": rollup.voltag,
        "drive_uuid": bytes_to_uuid_text(&rollup.drive_uuid),
        "drive_serial": rollup.drive_serial,
        "session_count": rollup.session_count,
        "snapshot_count": rollup.snapshot_count,
        "write_errors_corrected": rollup.write_errors_corrected,
        "write_errors_uncorrected": rollup.write_errors_uncorrected,
        "read_errors_corrected": rollup.read_errors_corrected,
        "read_errors_uncorrected": rollup.read_errors_uncorrected,
        "first_session_utc": timestamp_value(rollup.first_session_utc.as_ref()),
        "last_session_utc": timestamp_value(rollup.last_session_utc.as_ref()),
    })
}

fn drive_event_json(event: &pb::DriveHistoryEvent) -> Value {
    json!({
        "event_id": event.event_id,
        "drive_uuid": bytes_to_uuid_text(&event.drive_uuid),
        "event_kind": event.event_kind,
        "at_utc": timestamp_value(event.at_utc.as_ref()),
        "library_serial": event.library_serial,
        "element_address": event.element_address,
        "tape_uuid": bytes_to_uuid_text(&event.tape_uuid),
        "detail": event.detail,
    })
}

fn drive_snapshot_json(snapshot: &pb::DriveHealthSnapshot) -> Value {
    json!({
        "snapshot_id": snapshot.snapshot_id,
        "drive_uuid": bytes_to_uuid_text(&snapshot.drive_uuid),
        "at_utc": timestamp_value(snapshot.at_utc.as_ref()),
        "trigger": snapshot.trigger,
        "session_id": snapshot.session_id,
        "tape_alert_flags": snapshot.tape_alert_flags,
        "write_errors_corrected": snapshot.write_errors_corrected,
        "write_errors_uncorrected": snapshot.write_errors_uncorrected,
        "read_errors_corrected": snapshot.read_errors_corrected,
        "read_errors_uncorrected": snapshot.read_errors_uncorrected,
        "raw_pages": snapshot.raw_pages,
    })
}

fn alarm_json(alarm: &pb::Alarm) -> Value {
    json!({
        "alarm_id": alarm.alarm_id,
        "condition_key": alarm.condition_key,
        "kind": alarm.kind,
        "severity": alarm.severity,
        "state": alarm.state,
        "first_seen_utc": timestamp_value(alarm.first_seen_utc.as_ref()),
        "last_seen_utc": timestamp_value(alarm.last_seen_utc.as_ref()),
        "acked_by": alarm.acked_by,
        "acked_at_utc": timestamp_value(alarm.acked_at_utc.as_ref()),
        "detail": alarm.detail,
    })
}

fn drive_live_json(drive: &pb::Drive) -> Value {
    json!({
        "element_address": drive.element_address,
        "drive_serial": drive.drive_serial,
        "host_device_path": drive.host_device_path,
        "vendor": drive.vendor,
        "product": drive.product,
        "loaded_tape_uuid": if drive.loaded_tape_uuid.is_empty() {
            Value::Null
        } else {
            Value::String(bytes_to_uuid_text(&drive.loaded_tape_uuid))
        },
        "loaded_tape_barcode": if drive.loaded_tape_barcode.is_empty() {
            Value::Null
        } else {
            Value::String(drive.loaded_tape_barcode.clone())
        },
        "mount_age_seconds": drive.mount_age_seconds,
        "status": drive_status_name(drive.status),
        "drive_uuid": bytes_to_uuid_text(&drive.drive_uuid),
        "cleaning_due": drive.cleaning_due,
        "fenced": drive.fenced,
        "lifetime_read_bytes": drive.lifetime_read_bytes,
        "lifetime_write_bytes": drive.lifetime_write_bytes,
        "counter_epoch": drive.counter_epoch,
        "session_id": if drive.session_id.is_empty() {
            Value::Null
        } else {
            Value::String(bytes_to_uuid_text(&drive.session_id))
        },
        "active_alert_names": drive.active_alert_names,
        "tape_io": {
            "staging_ring_buffers": drive.tape_io_staging_ring_buffers,
            "effective_batch_blocks": drive.tape_io_effective_batch_blocks,
            "gap_p50_us": drive.tape_io_gap_p50_us,
            "gap_p95_us": drive.tape_io_gap_p95_us,
            "gap_max_us": drive.tape_io_gap_max_us,
            "ioctl_p50_us": drive.tape_io_ioctl_p50_us,
            "ioctl_p95_us": drive.tape_io_ioctl_p95_us,
            "ioctl_max_us": drive.tape_io_ioctl_max_us,
            "cadence_us": drive.tape_io_cadence_us,
            "window_feed_bytes_per_second": drive.tape_io_window_feed_bytes_per_second,
            "session_average_feed_bytes_per_second": drive.tape_io_effective_feed_bytes_per_second,
        },
    })
}

fn library_state_json(state: &pb::LibraryState) -> Value {
    json!({
        "library": state.library.as_ref().map(|library| json!({
            "library_serial": library.library_serial,
            "vendor": library.vendor,
            "product": library.product,
            "product_revision": library.product_revision,
            "library_uuid": bytes_to_uuid_text(&library.library_uuid),
        })),
        "drives": state.drives.iter().map(drive_live_json).collect::<Vec<_>>(),
        "slots": state.slots.iter().map(|slot| {
            json!({
                "element_address": slot.element_address,
                "voltag": slot.voltag,
                "tape_uuid": if slot.tape_uuid.is_empty() {
                    Value::Null
                } else {
                    Value::String(bytes_to_uuid_text(&slot.tape_uuid))
                },
            })
        }).collect::<Vec<_>>(),
        "import_export_ports": state.import_export_ports.iter().map(|port| {
            json!({
                "element_address": port.element_address,
                "voltag": port.voltag,
                "tape_uuid": if port.tape_uuid.is_empty() {
                    Value::Null
                } else {
                    Value::String(bytes_to_uuid_text(&port.tape_uuid))
                },
            })
        }).collect::<Vec<_>>(),
        "last_inventory_at": timestamp_value(state.last_inventory_at.as_ref()),
        "managed": state.managed,
    })
}

fn print_live_status_json(
    response: &pb::GetLiveStatusResponse,
    out: &mut dyn Write,
) -> Result<(), String> {
    let data = json!({
        "libraries": response.libraries.iter().map(library_state_json).collect::<Vec<_>>(),
        "operations": response.operations.iter().map(|operation| {
            json!({
                "operation_id": bytes_to_uuid_text(&operation.operation_id),
            })
        }).collect::<Vec<_>>(),
        "alarms": response.alarms.iter().map(alarm_json).collect::<Vec<_>>(),
        "snapshot_at_utc": response.snapshot_at_utc,
        "daemon_epoch": response.daemon_epoch,
    });
    print_json_envelope("rem.top.v1", "item", data, out)
}

fn print_live_status_text(
    response: &pb::GetLiveStatusResponse,
    out: &mut dyn Write,
) -> Result<(), String> {
    let _ = writeln!(
        out,
        "snapshot_at_utc: {}  daemon_epoch: {}",
        response.snapshot_at_utc, response.daemon_epoch
    );
    for library in &response.libraries {
        let serial = library
            .library
            .as_ref()
            .map(|value| value.library_serial.as_str())
            .unwrap_or("<unknown>");
        let managed = library.managed.as_str();
        let _ = writeln!(out, "library {serial} [{managed}]");
        for drive in &library.drives {
            let tape = drive.loaded_tape_barcode.as_str();
            let tape = if tape.is_empty() { "-" } else { tape };
            let _ = writeln!(
                out,
                "  bay {bay:04x} serial={serial} tape_barcode={tape} mount_age_s={mount_age} state={state} read={read} write={write} epoch={epoch} ring={ring} gap_p95_us={gap_p95} feed_window_Bps={window_feed} feed_session_avg_Bps={session_feed}",
                bay = drive.element_address,
                serial = drive.drive_serial,
                state = drive_status_name(drive.status),
                read = drive.lifetime_read_bytes,
                write = drive.lifetime_write_bytes,
                epoch = drive.counter_epoch,
                ring = drive.tape_io_staging_ring_buffers,
                gap_p95 = drive.tape_io_gap_p95_us,
                window_feed = drive.tape_io_window_feed_bytes_per_second,
                session_feed = drive.tape_io_effective_feed_bytes_per_second,
                mount_age = drive.mount_age_seconds,
            );
        }
    }
    Ok(())
}

fn tape_file_json(file: &pb::TapeFile) -> Value {
    json!({
        "tape_uuid": bytes_to_uuid_text(&file.tape_uuid),
        "tape_file_number": file.tape_file_number,
        "kind": file.kind,
        "block_count": file.block_count,
        "object_id": if file.object_id.is_empty() {
            Value::Null
        } else {
            Value::String(bytes_to_uuid_text(&file.object_id))
        },
    })
}

fn tape_pool_json(pool: &pb::TapePool) -> Value {
    json!({
        "pool_id": pool.pool_id,
        "display_name": pool.display_name,
        "copy_class": pool.copy_class,
        "content_class": pool.content_class,
    })
}

fn catalog_unit_json(unit: &pb::CatalogUnit) -> Value {
    json!({
        "unit_id": bytes_to_text_or_uuid(&unit.unit_id),
        "tape_uuid": bytes_to_uuid_text(&unit.tape_uuid),
        "format_id": unit.format_id,
        "origin_kind": catalog_unit_origin_name(unit.origin_kind),
        "discovered_at": timestamp_value(unit.discovered_at.as_ref()),
    })
}

fn catalog_entry_json(entry: &pb::CatalogEntry) -> Value {
    json!({
        "unit_id": bytes_to_text_or_uuid(&entry.unit_id),
        "entry_id": bytes_to_text_or_uuid(&entry.entry_id),
        "path": entry.path,
        "kind": catalog_entry_kind_name(entry.kind),
        "size_bytes": entry.size_bytes,
        "mtime": timestamp_value(entry.mtime.as_ref()),
        "state": catalog_entry_state_name(entry.state),
        "integrity_basis": integrity_basis_name(entry.integrity_basis),
    })
}

fn timestamp_value(timestamp: Option<&prost_types::Timestamp>) -> Value {
    timestamp_text(timestamp)
        .map(Value::String)
        .unwrap_or(Value::Null)
}

fn timestamp_text(timestamp: Option<&prost_types::Timestamp>) -> Option<String> {
    let timestamp = timestamp?;
    let base = OffsetDateTime::from_unix_timestamp(timestamp.seconds).ok()?;
    let datetime = base.checked_add(Duration::nanoseconds(timestamp.nanos as i64))?;
    datetime.format(&Rfc3339).ok()
}

fn bytes_to_uuid_text(bytes: &[u8]) -> String {
    <[u8; 16]>::try_from(bytes)
        .map(|bytes| Uuid::from_bytes(bytes).to_string())
        .unwrap_or_else(|_| bytes_to_hex(bytes))
}

fn bytes_to_text_or_uuid(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap_or_else(|_| bytes_to_uuid_text(bytes))
}

pub(crate) fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut out, "{byte:02x}").expect("write to string");
    }
    out
}

fn dash_if_empty(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

fn health_status_name(value: i32) -> &'static str {
    match value {
        1 => "healthy",
        2 => "read_only",
        3 => "degraded",
        4 => "failed",
        _ => "unspecified",
    }
}

fn operation_state_name(value: i32) -> &'static str {
    match value {
        1 => "queued",
        2 => "running",
        3 => "succeeded",
        4 => "failed",
        5 => "cancelled",
        6 => "unknown",
        _ => "unspecified",
    }
}

fn tape_state_name(value: i32) -> &'static str {
    match value {
        1 => "inventoried",
        2 => "ready",
        3 => "degraded",
        4 => "failed",
        _ => "unspecified",
    }
}

pub fn drive_status_name(value: i32) -> String {
    match value {
        1 => "idle".to_string(),
        2 => "loaded".to_string(),
        3 => "busy".to_string(),
        4 => "unreachable".to_string(),
        5 => "cleaning".to_string(),
        6 => "fenced".to_string(),
        0 => "unspecified".to_string(),
        other => format!("unknown({other})"),
    }
}

fn catalog_unit_origin_name(value: i32) -> &'static str {
    match value {
        1 => "native_object",
        2 => "foreign_archive",
        _ => "unspecified",
    }
}

fn catalog_entry_kind_name(value: i32) -> &'static str {
    match value {
        1 => "regular_file",
        2 => "directory",
        3 => "symlink",
        4 => "hardlink",
        5 => "special",
        _ => "unspecified",
    }
}

fn catalog_entry_state_name(value: i32) -> &'static str {
    match value {
        1 => "complete",
        2 => "partial",
        3 => "damaged",
        4 => "unsupported",
        5 => "unknown",
        _ => "unspecified",
    }
}

fn integrity_basis_name(value: i32) -> &'static str {
    match value {
        1 => "unknown",
        2 => "content_hash",
        3 => "format_checksum",
        4 => "parity_consistency",
        _ => "unspecified",
    }
}

fn run_rebuild_catalog_from_journals(
    config: &PathBuf,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let mut handle = match remanence_state::StateHandle::open_from_config_file(config) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
    };
    match handle.rebuild_index_from_journals() {
        Ok(report) => {
            let _ = writeln!(out, "rebuild-catalog-from-journals ok");
            let _ = writeln!(out, "  tapes: {}", report.tapes_rebuilt);
            let _ = writeln!(out, "  tape files: {}", report.tape_files_rebuilt);
            let _ = writeln!(out, "  object copies: {}", report.object_copies_rebuilt);
            let _ = writeln!(out, "  audit records: {}", report.audit_records_replayed);
            let _ = writeln!(out, "  tape journals: {}", report.journal_records_replayed);
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_local_catalog_command(
    command: &LocalCatalogCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match command {
        LocalCatalogCommand::Reset(args) => run_catalog_reset(args, out, err),
    }
}

fn run_catalog_reset(
    args: &CatalogResetArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    if !args.i_understand_this_erases_the_catalog {
        let _ = writeln!(
            err,
            "error: catalog reset erases the catalog, audit history, drive history, drive health snapshots, cleaning runs, and alarms; pass --i-understand-this-erases-the-catalog"
        );
        return ExitCode::from(2);
    }

    match remanence_state::StateHandle::reset_catalog_from_config_file(&args.config) {
        Ok(()) => {
            let _ = writeln!(out, "catalog reset ok");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            ExitCode::from(1)
        }
    }
}

/// Outcome facts shared by the retire JSON envelope and the human one-liner.
struct TapeRetireReport {
    tape_uuid: Vec<u8>,
    voltag: Option<String>,
    newly_retired: bool,
    copies_marked_missing: u64,
    degraded_objects: Vec<String>,
}

fn run_tape_retire(args: &TapeRetireArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let config = match remanence_state::load_config(&args.config) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
    };
    let paths = remanence_state::StatePaths::from_config(&args.config, &config);

    let report = if args.dry_run {
        // Dry-run never takes the state lock or opens the audit log: it
        // reads the catalog and computes the would-be outcome.
        let catalog = match remanence_state::CatalogIndex::open_read_only(&paths.sqlite_path) {
            Ok(catalog) => catalog,
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                return ExitCode::from(1);
            }
        };
        match preview_tape_retire(&catalog, args.target.as_str()) {
            Ok(report) => report,
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                return ExitCode::from(1);
            }
        }
    } else {
        // `StateHandle::open` takes the exclusive state flock, so this fails
        // cleanly while the daemon runs; the recycle workflow stops the
        // daemon before retire + init.
        let mut state = match remanence_state::StateHandle::open_with_config(paths, config) {
            Ok(state) => state,
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                return ExitCode::from(1);
            }
        };
        match apply_tape_retire(&mut state, args.target.as_str(), args.reason.as_str()) {
            Ok(report) => report,
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                return ExitCode::from(1);
            }
        }
    };

    if args.json {
        let data = json!({
            "tape_uuid": bytes_to_uuid_text(&report.tape_uuid),
            "voltag": report.voltag,
            "reason": args.reason,
            "dry_run": args.dry_run,
            "newly_retired": report.newly_retired,
            "copies_marked_missing": report.copies_marked_missing,
            "degraded_objects": report.degraded_objects,
        });
        if let Err(error) = print_json_envelope("rem.tape.retire.v1", "item", data, out) {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }

    print_tape_retire_report(&report, args.dry_run, out, err);
    ExitCode::SUCCESS
}

fn print_tape_retire_report(
    report: &TapeRetireReport,
    dry_run: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) {
    let uuid = bytes_to_uuid_text(&report.tape_uuid);
    let label = match report.voltag.as_deref() {
        Some(voltag) => format!("{voltag} (uuid {uuid})"),
        None => format!("uuid {uuid}"),
    };
    let copies = count_label(report.copies_marked_missing, "copy", "copies");
    let objects = count_label(report.degraded_objects.len() as u64, "object", "objects");
    if !report.newly_retired {
        let _ = writeln!(
            out,
            "{}{label} is already retired; no change",
            if dry_run { "dry-run: " } else { "" }
        );
        return;
    }
    if dry_run {
        let _ = writeln!(
            out,
            "dry-run: would retire {label}: {copies} would be marked missing, \
             {objects} would become degraded"
        );
        for object_id in &report.degraded_objects {
            let _ = writeln!(out, "  would lose last committed copy: {object_id}");
        }
        return;
    }
    let _ = writeln!(
        out,
        "retired {label}: {copies} marked missing, {objects} now degraded"
    );
    if !report.degraded_objects.is_empty() {
        let _ = writeln!(
            err,
            "warning: {objects} lost their last committed copy: {}",
            report.degraded_objects.join(", ")
        );
    }
}

fn count_label(count: u64, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn run_tape_quarantine(
    command: &TapeQuarantineCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match command {
        TapeQuarantineCommand::List {
            config,
            library,
            json,
        } => {
            let catalog = match open_catalog_read_only_for_config(config, err) {
                Ok(catalog) => catalog,
                Err(code) => return code,
            };
            let records = match catalog.list_active_media_readiness_operations(library.as_deref()) {
                Ok(records) => records,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return ExitCode::from(1);
                }
            };
            print_tape_quarantine_list(&records, *json, out, err)
        }
        TapeQuarantineCommand::Show {
            quarantine,
            config,
            json,
        } => {
            let catalog = match open_catalog_read_only_for_config(config, err) {
                Ok(catalog) => catalog,
                Err(code) => return code,
            };
            let records = match catalog.list_active_media_readiness_operations(None) {
                Ok(records) => records,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return ExitCode::from(1);
                }
            };
            let record = match find_media_readiness_quarantine(&records, quarantine.as_str()) {
                Ok(record) => record,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return ExitCode::from(1);
                }
            };
            print_tape_quarantine_show(record, *json, out, err)
        }
        TapeQuarantineCommand::Release {
            quarantine,
            config,
            ack,
            json,
            ..
        } => {
            let (paths, config) = match load_state_config(config, err) {
                Ok(value) => value,
                Err(code) => return code,
            };
            let mut state = match remanence_state::StateHandle::open_with_config(paths, config) {
                Ok(state) => state,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return ExitCode::from(1);
                }
            };
            let records = match state
                .catalog_index()
                .list_active_media_readiness_operations(None)
            {
                Ok(records) => records,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return ExitCode::from(1);
                }
            };
            let record = match find_media_readiness_quarantine(&records, quarantine.as_str()) {
                Ok(record) => record.clone(),
                Err(_) => {
                    match state
                        .catalog_index()
                        .release_tape_io_fence(quarantine.as_str(), ack.trim())
                    {
                        Ok(Some(record)) => {
                            return print_tape_io_quarantine_released(&record, *json, out, err);
                        }
                        Ok(None) => {
                            let _ = writeln!(
                                err,
                                "error: no active media-readiness or tape-I/O quarantine {:?}",
                                quarantine
                            );
                            return ExitCode::from(1);
                        }
                        Err(error) => {
                            let _ = writeln!(err, "error: {error}");
                            return ExitCode::from(1);
                        }
                    }
                }
            };
            let operation_id = match Uuid::parse_str(record.operation_id.as_str()) {
                Ok(operation_id) => operation_id,
                Err(error) => {
                    let _ = writeln!(
                        err,
                        "error: media readiness operation {} is not a UUID: {error}",
                        record.operation_id
                    );
                    return ExitCode::from(1);
                }
            };
            let released = match state.catalog_index().record_media_readiness_transition(
                media_readiness_release_transition(operation_id, ack.trim()),
            ) {
                Ok(record) => record,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return ExitCode::from(1);
                }
            };
            print_tape_quarantine_released(&released, *json, out, err)
        }
    }
}

fn tape_io_fence_json(record: &remanence_state::TapeIoFenceRecord) -> Value {
    json!({
        "quarantine_id": record.quarantine_id.as_str(),
        "fence_id": record.fence_id,
        "tape_uuid": bytes_to_uuid_text(&record.tape_uuid),
        "barcode": record.barcode.as_deref(),
        "state": record.state.as_str(),
        "reason": record.reason.as_str(),
        "evidence_json": record.evidence_json.as_deref(),
        "created_at_utc": record.created_at_utc.as_str(),
        "updated_at_utc": record.updated_at_utc.as_str(),
        "release_ack": record.release_ack.as_deref(),
    })
}

fn print_tape_io_quarantine_released(
    record: &remanence_state::TapeIoFenceRecord,
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    if json_output {
        if let Err(error) = print_json_envelope(
            "rem.tape.quarantine.release.v1",
            "item",
            tape_io_fence_json(record),
            out,
        ) {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }
    let _ = writeln!(
        out,
        "released {} tape_uuid={} barcode={} reason={}",
        record.quarantine_id,
        bytes_to_uuid_text(&record.tape_uuid),
        record.barcode.as_deref().unwrap_or("(unknown)"),
        record.reason
    );
    ExitCode::SUCCESS
}

fn load_state_config(
    config_path: &Path,
    err: &mut dyn Write,
) -> Result<(remanence_state::StatePaths, remanence_state::RemConfig), ExitCode> {
    let config = match remanence_state::load_config(config_path) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return Err(ExitCode::from(1));
        }
    };
    let paths = remanence_state::StatePaths::from_config(config_path, &config);
    Ok((paths, config))
}

fn open_catalog_read_only_for_config(
    config_path: &Path,
    err: &mut dyn Write,
) -> Result<remanence_state::CatalogIndex, ExitCode> {
    let (paths, _) = load_state_config(config_path, err)?;
    remanence_state::CatalogIndex::open_read_only(&paths.sqlite_path).map_err(|error| {
        let _ = writeln!(err, "error: {error}");
        ExitCode::from(1)
    })
}

fn media_readiness_quarantine_id(
    record: &remanence_state::MediaReadinessOperationRecord,
) -> String {
    record
        .quarantine_id
        .clone()
        .unwrap_or_else(|| format!("mrq-{}", record.operation_id))
}

fn find_media_readiness_quarantine<'a>(
    records: &'a [remanence_state::MediaReadinessOperationRecord],
    selector: &str,
) -> Result<&'a remanence_state::MediaReadinessOperationRecord, String> {
    let selector = selector.trim();
    let matches = records
        .iter()
        .filter(|record| {
            record.operation_id == selector || media_readiness_quarantine_id(record) == selector
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [record] => Ok(*record),
        [] => Err(format!("no active media-readiness quarantine {selector:?}")),
        _ => Err(format!(
            "media-readiness quarantine selector {selector:?} is ambiguous"
        )),
    }
}

fn media_readiness_record_json(record: &remanence_state::MediaReadinessOperationRecord) -> Value {
    json!({
        "quarantine_id": media_readiness_quarantine_id(record),
        "operation_id": record.operation_id.as_str(),
        "run_id": record.run_id.as_deref(),
        "library_serial": record.library_serial.as_str(),
        "changer_sg": record.changer_sg.as_deref(),
        "drive_element": format!("0x{:04x}", record.drive_element),
        "drive_element_raw": record.drive_element,
        "drive_sg": record.drive_sg.as_deref(),
        "drive_serial": record.drive_serial.as_deref(),
        "barcode": record.barcode.as_deref(),
        "source_slot": record.source_slot.map(|slot| format!("0x{slot:04x}")),
        "source_slot_raw": record.source_slot,
        "media_generation": record.media_generation,
        "phase": record.phase.as_str(),
        "state": record.state.as_str(),
        "dirty_scope": record.dirty_scope.as_deref(),
        "started_at_utc": record.started_at_utc.as_str(),
        "updated_at_utc": record.updated_at_utc.as_str(),
        "deadline_at_utc": record.deadline_at_utc.as_deref(),
        "last_cdb_opcode": record.last_cdb_opcode,
        "last_sense_raw": record.last_sense_raw.as_deref(),
        "last_sense_key": record.last_sense_key,
        "last_asc": record.last_asc,
        "last_ascq": record.last_ascq,
        "last_host_status": record.last_host_status,
        "last_driver_status": record.last_driver_status,
        "target_status": record.target_status,
        "transport_class": record.transport_class.as_deref(),
        "cancel_source": record.cancel_source.as_deref(),
        "signal": record.signal.as_deref(),
        "evidence_path": record.evidence_path.as_deref(),
        "last_error_json": record.last_error_json.as_deref(),
    })
}

fn print_tape_quarantine_list(
    records: &[remanence_state::MediaReadinessOperationRecord],
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    if json_output {
        let items = records
            .iter()
            .map(media_readiness_record_json)
            .collect::<Vec<_>>();
        if let Err(error) =
            print_json_envelope("rem.tape.quarantine.list.v1", "list", json!(items), out)
        {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }
    if records.is_empty() {
        let _ = writeln!(out, "(no active media-readiness quarantines)");
        return ExitCode::SUCCESS;
    }
    for record in records {
        let _ = writeln!(
            out,
            "{} operation={} library={} drive=0x{:04x} barcode={} state={} updated={}",
            media_readiness_quarantine_id(record),
            record.operation_id,
            record.library_serial,
            record.drive_element,
            record.barcode.as_deref().unwrap_or("(unknown)"),
            record.state,
            record.updated_at_utc
        );
    }
    ExitCode::SUCCESS
}

fn print_tape_quarantine_show(
    record: &remanence_state::MediaReadinessOperationRecord,
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    if json_output {
        if let Err(error) = print_json_envelope(
            "rem.tape.quarantine.show.v1",
            "item",
            media_readiness_record_json(record),
            out,
        ) {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }
    let _ = writeln!(out, "quarantine: {}", media_readiness_quarantine_id(record));
    let _ = writeln!(out, "operation: {}", record.operation_id);
    let _ = writeln!(out, "library: {}", record.library_serial);
    let _ = writeln!(out, "drive: 0x{:04x}", record.drive_element);
    let _ = writeln!(
        out,
        "barcode: {}",
        record.barcode.as_deref().unwrap_or("(unknown)")
    );
    let _ = writeln!(out, "state: {}", record.state);
    let _ = writeln!(
        out,
        "dirty_scope: {}",
        record.dirty_scope.as_deref().unwrap_or("(unknown)")
    );
    let _ = writeln!(out, "updated: {}", record.updated_at_utc);
    ExitCode::SUCCESS
}

fn print_tape_quarantine_released(
    record: &remanence_state::MediaReadinessOperationRecord,
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    if json_output {
        if let Err(error) = print_json_envelope(
            "rem.tape.quarantine.release.v1",
            "item",
            media_readiness_record_json(record),
            out,
        ) {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
        return ExitCode::SUCCESS;
    }
    let _ = writeln!(
        out,
        "released {} operation={} library={} drive=0x{:04x} barcode={}",
        media_readiness_quarantine_id(record),
        record.operation_id,
        record.library_serial,
        record.drive_element,
        record.barcode.as_deref().unwrap_or("(unknown)")
    );
    ExitCode::SUCCESS
}

fn media_readiness_admission_error(
    action: &str,
    conflicts: &[remanence_state::MediaReadinessOperationRecord],
) -> String {
    let Some(first) = conflicts.first() else {
        return "internal error: media-readiness admission called without conflicts".to_string();
    };
    format!(
        "{action} is blocked by active media-readiness fence {} operation={} library={} drive=0x{:04x} barcode={} state={}; run `rem tape quarantine show {}` or wait-ready/resume before retrying",
        media_readiness_quarantine_id(first),
        first.operation_id,
        first.library_serial,
        first.drive_element,
        first.barcode.as_deref().unwrap_or("(unknown)"),
        first.state,
        media_readiness_quarantine_id(first)
    )
}

/// Resolve a retire target: barcode first, then 32-hex tape uuid.
///
/// LTO voltags are 6-8 characters, so there is no collision space with a
/// 32-hex uuid.
fn resolve_tape_retire_target(
    catalog: &remanence_state::CatalogIndex,
    target: &str,
) -> Result<remanence_state::TapeRecord, String> {
    if let Some(record) = catalog
        .get_tape_by_voltag(target)
        .map_err(|error| format!("look up tape by voltag: {error}"))?
    {
        return Ok(record);
    }
    let uuid = Uuid::parse_str(target.trim()).map_err(|_| {
        format!("no tape with voltag {target:?}, and {target:?} is not a tape uuid")
    })?;
    catalog
        .get_tape(uuid.as_bytes())
        .map_err(|error| format!("look up tape by uuid: {error}"))?
        .ok_or_else(|| format!("no tape with voltag or uuid {target:?}"))
}

fn preview_tape_retire(
    catalog: &remanence_state::CatalogIndex,
    target: &str,
) -> Result<TapeRetireReport, String> {
    let record = resolve_tape_retire_target(catalog, target)?;
    if record.state == "retired" {
        return Ok(TapeRetireReport {
            tape_uuid: record.tape_uuid,
            voltag: record.voltag,
            newly_retired: false,
            copies_marked_missing: 0,
            degraded_objects: Vec::new(),
        });
    }
    let objects = catalog
        .list_native_objects()
        .map_err(|error| format!("list objects for retire preview: {error}"))?;
    let mut copies_marked_missing = 0u64;
    let mut degraded_objects = Vec::new();
    for object in objects {
        let mut committed_on_tape = 0u64;
        let mut committed_elsewhere = false;
        for copy in &object.copies {
            if copy.status != "committed" {
                continue;
            }
            if copy.tape_uuid == record.tape_uuid {
                committed_on_tape += 1;
            } else {
                committed_elsewhere = true;
            }
        }
        copies_marked_missing += committed_on_tape;
        if committed_on_tape > 0 && !committed_elsewhere {
            degraded_objects.push(object.object_id);
        }
    }
    degraded_objects.sort();
    Ok(TapeRetireReport {
        tape_uuid: record.tape_uuid,
        voltag: record.voltag,
        newly_retired: true,
        copies_marked_missing,
        degraded_objects,
    })
}

fn apply_tape_retire(
    state: &mut remanence_state::StateHandle,
    target: &str,
    reason: &str,
) -> Result<TapeRetireReport, String> {
    let record = resolve_tape_retire_target(state.catalog_index(), target)?;
    let tape_uuid: [u8; 16] = record
        .tape_uuid
        .as_slice()
        .try_into()
        .map_err(|_| format!("catalog tape row has {} UUID bytes", record.tape_uuid.len()))?;
    let degraded_before = state
        .catalog_index()
        .list_objects_with_no_committed_copies()
        .map_err(|error| format!("query degraded objects before retire: {error}"))?;
    let outcome = state
        .retire_tape(remanence_state::RetireTapeInput {
            tape_uuid,
            reason: reason.to_string(),
        })
        .map_err(|error| format!("retire tape: {error}"))?;
    let degraded_after = state
        .catalog_index()
        .list_objects_with_no_committed_copies()
        .map_err(|error| format!("query degraded objects after retire: {error}"))?;
    let mut degraded_objects = degraded_after
        .into_iter()
        .filter(|object_id| !degraded_before.contains(object_id))
        .collect::<Vec<_>>();
    degraded_objects.sort();
    Ok(TapeRetireReport {
        tape_uuid: record.tape_uuid,
        voltag: outcome.released_voltag,
        newly_retired: outcome.newly_retired,
        copies_marked_missing: outcome.copies_marked_missing,
        degraded_objects,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TapeInitLocation {
    Slot,
    Drive,
}

#[derive(Clone, Debug)]
struct TapeInitCandidate {
    library_serial: String,
    element_address: u16,
    location: TapeInitLocation,
    voltag: Option<String>,
}

impl TapeInitCandidate {
    fn label(&self) -> String {
        match self.voltag.as_deref() {
            Some(voltag) => format!(
                "{voltag} in {} 0x{:04x}",
                self.location_label(),
                self.element_address
            ),
            None => format!("{} 0x{:04x}", self.location_label(), self.element_address),
        }
    }

    fn location_label(&self) -> &'static str {
        match self.location {
            TapeInitLocation::Slot => "slot",
            TapeInitLocation::Drive => "drive",
        }
    }
}

struct TapeInitRunResult {
    success: bool,
    uuid: remanence_api::TapeUuid,
    voltag: String,
    pool_id: String,
    generation: remanence_api::LtoGen,
    capacity_bytes: u64,
    block_size: u32,
    parity: remanence_api::ParityConfig,
    library_serial: String,
    source_element: u16,
    drive_element: u16,
    decision: remanence_api::InitDecision,
    action: remanence_api::TapeInitWriteAction,
    notes: Vec<String>,
}

struct TapeInitHardwarePlan {
    candidate: TapeInitCandidate,
    voltag: String,
    pool_id: String,
    generation: remanence_api::LtoGen,
    fresh_block_size: u32,
    drive_element: u16,
}

struct TapeInitRunContext<'a> {
    report: &'a DiscoveryReport,
    config: &'a remanence_state::RemConfig,
    policy: &'a StaticAllowlist,
    args: &'a TapeInitArgs,
}

trait TapeInitStateOps {
    fn project_catalog_inputs(
        &mut self,
        voltag: &str,
        bot: &remanence_api::BotClassification,
        pool_id: &str,
    ) -> Result<remanence_api::TapeInitCatalogProjection, String>;

    fn provision_initialized_tape(
        &mut self,
        config: &remanence_state::RemConfig,
        tape_uuid: remanence_api::TapeUuid,
        voltag: String,
        block_size: u32,
        parity: remanence_api::ParityConfig,
        force: bool,
    ) -> Result<(), String>;

    fn record_media_readiness_operation(
        &mut self,
        input: remanence_state::MediaReadinessOperationInput,
    ) -> Result<(), String> {
        let _ = input;
        Ok(())
    }

    fn record_media_readiness_transition(
        &mut self,
        input: remanence_state::MediaReadinessTransitionInput,
    ) -> Result<(), String> {
        let _ = input;
        Ok(())
    }

    fn media_readiness_admission_conflicts(
        &mut self,
        library_serial: &str,
        drive_element: Option<u16>,
        barcode: Option<&str>,
        library_robotics: bool,
    ) -> Result<Vec<remanence_state::MediaReadinessOperationRecord>, String>;
}

impl TapeInitStateOps for remanence_state::StateHandle {
    fn project_catalog_inputs(
        &mut self,
        voltag: &str,
        bot: &remanence_api::BotClassification,
        pool_id: &str,
    ) -> Result<remanence_api::TapeInitCatalogProjection, String> {
        remanence_api::project_tape_init_catalog_inputs(self.catalog_index(), voltag, bot, pool_id)
            .map_err(|error| format!("project catalog init inputs: {error}"))
    }

    fn provision_initialized_tape(
        &mut self,
        config: &remanence_state::RemConfig,
        tape_uuid: remanence_api::TapeUuid,
        voltag: String,
        block_size: u32,
        parity: remanence_api::ParityConfig,
        force: bool,
    ) -> Result<(), String> {
        self.catalog_index()
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: voltag.clone(),
                block_size,
                parity: parity.clone(),
                force,
            })
            .map_err(|error| format!("provision catalog tape row: {error}"))?;
        let pool_inputs = pool_projection_inputs(config);
        self.catalog_index()
            .reconcile_tape_pool_projection_from_rules(&pool_inputs, &config.tape_pool_rules)
            .map_err(|error| format!("project tape pool membership: {error}"))?;
        // Tape init's only state-changing success path runs through here
        // (the CLI calls this when `action == WroteBootstrap`, never for
        // idempotent no-ops or refusals), so a `TapeProvisioned` append here
        // is exactly "a bootstrap was actually written".
        append_tape_provisioned_audit_event(self, tape_uuid, &voltag, block_size, &parity, force)
    }

    fn record_media_readiness_operation(
        &mut self,
        input: remanence_state::MediaReadinessOperationInput,
    ) -> Result<(), String> {
        self.catalog_index()
            .record_media_readiness_operation(input)
            .map(|_| ())
            .map_err(|error| format!("record media readiness operation: {error}"))
    }

    fn record_media_readiness_transition(
        &mut self,
        input: remanence_state::MediaReadinessTransitionInput,
    ) -> Result<(), String> {
        self.catalog_index()
            .record_media_readiness_transition(input)
            .map(|_| ())
            .map_err(|error| format!("record media readiness transition: {error}"))
    }

    fn media_readiness_admission_conflicts(
        &mut self,
        library_serial: &str,
        drive_element: Option<u16>,
        barcode: Option<&str>,
        library_robotics: bool,
    ) -> Result<Vec<remanence_state::MediaReadinessOperationRecord>, String> {
        self.catalog_index()
            .media_readiness_admission_conflicts(
                library_serial,
                drive_element,
                barcode,
                library_robotics,
            )
            .map_err(|error| format!("check media readiness admission: {error}"))
    }
}

fn append_tape_provisioned_audit_event(
    state: &mut remanence_state::StateHandle,
    tape_uuid: remanence_api::TapeUuid,
    voltag: &str,
    block_size: u32,
    parity: &remanence_api::ParityConfig,
    forced: bool,
) -> Result<(), String> {
    use ciborium::value::Value as CborValue;

    let geometry = match parity {
        remanence_api::ParityConfig::None => "no-parity".to_string(),
        remanence_api::ParityConfig::Scheme(scheme) => format!(
            "scheme={} data={} parity={} stripes={}",
            scheme.id.as_str(),
            scheme.data_blocks_per_stripe,
            scheme.parity_blocks_per_stripe,
            scheme.stripes_per_neighborhood
        ),
    };
    let detail = std::collections::BTreeMap::from([
        ("voltag".to_string(), CborValue::Text(voltag.to_string())),
        (
            "block_size".to_string(),
            CborValue::Integer(block_size.into()),
        ),
        ("geometry".to_string(), CborValue::Text(geometry)),
        ("forced".to_string(), CborValue::Bool(forced)),
    ]);
    state
        .audit()
        .append(remanence_state::AuditEventRecord {
            actor: remanence_state::AuditActor::local_user(),
            source_layer: remanence_state::SourceLayer::Layer4,
            operation_id: None,
            session_id: None,
            idempotency_key: None,
            event: remanence_state::AuditEvent::TapeProvisioned,
            subject: remanence_state::AuditSubject {
                kind: "tape".to_string(),
                id: Some(bytes_to_hex(&tape_uuid)),
            },
            detail,
        })
        .map_err(|error| format!("append tape provisioning audit record: {error}"))?;
    Ok(())
}

impl TapeInitStateOps for remanence_state::CatalogIndex {
    fn project_catalog_inputs(
        &mut self,
        voltag: &str,
        bot: &remanence_api::BotClassification,
        pool_id: &str,
    ) -> Result<remanence_api::TapeInitCatalogProjection, String> {
        remanence_api::project_tape_init_catalog_inputs(self, voltag, bot, pool_id)
            .map_err(|error| format!("project catalog init inputs: {error}"))
    }

    fn provision_initialized_tape(
        &mut self,
        _config: &remanence_state::RemConfig,
        _tape_uuid: remanence_api::TapeUuid,
        _voltag: String,
        _block_size: u32,
        _parity: remanence_api::ParityConfig,
        _force: bool,
    ) -> Result<(), String> {
        Err("internal error: dry-run attempted to provision catalog state".to_string())
    }

    fn record_media_readiness_operation(
        &mut self,
        input: remanence_state::MediaReadinessOperationInput,
    ) -> Result<(), String> {
        remanence_state::CatalogIndex::record_media_readiness_operation(self, input)
            .map(|_| ())
            .map_err(|error| format!("record media readiness operation: {error}"))
    }

    fn record_media_readiness_transition(
        &mut self,
        input: remanence_state::MediaReadinessTransitionInput,
    ) -> Result<(), String> {
        remanence_state::CatalogIndex::record_media_readiness_transition(self, input)
            .map(|_| ())
            .map_err(|error| format!("record media readiness transition: {error}"))
    }

    fn media_readiness_admission_conflicts(
        &mut self,
        library_serial: &str,
        drive_element: Option<u16>,
        barcode: Option<&str>,
        library_robotics: bool,
    ) -> Result<Vec<remanence_state::MediaReadinessOperationRecord>, String> {
        remanence_state::CatalogIndex::media_readiness_admission_conflicts(
            self,
            library_serial,
            drive_element,
            barcode,
            library_robotics,
        )
        .map_err(|error| format!("check media readiness admission: {error}"))
    }
}

struct DryRunTapeInitState {
    catalog: remanence_state::CatalogIndex,
}

impl DryRunTapeInitState {
    fn new(catalog: remanence_state::CatalogIndex) -> Self {
        Self { catalog }
    }
}

impl TapeInitStateOps for DryRunTapeInitState {
    fn project_catalog_inputs(
        &mut self,
        voltag: &str,
        bot: &remanence_api::BotClassification,
        pool_id: &str,
    ) -> Result<remanence_api::TapeInitCatalogProjection, String> {
        remanence_api::project_tape_init_catalog_inputs(&self.catalog, voltag, bot, pool_id)
            .map_err(|error| format!("project catalog init inputs: {error}"))
    }

    fn provision_initialized_tape(
        &mut self,
        _config: &remanence_state::RemConfig,
        _tape_uuid: remanence_api::TapeUuid,
        _voltag: String,
        _block_size: u32,
        _parity: remanence_api::ParityConfig,
        _force: bool,
    ) -> Result<(), String> {
        Err("internal error: dry-run attempted to provision catalog state".to_string())
    }

    fn media_readiness_admission_conflicts(
        &mut self,
        library_serial: &str,
        drive_element: Option<u16>,
        barcode: Option<&str>,
        library_robotics: bool,
    ) -> Result<Vec<remanence_state::MediaReadinessOperationRecord>, String> {
        self.catalog
            .media_readiness_admission_conflicts(
                library_serial,
                drive_element,
                barcode,
                library_robotics,
            )
            .map_err(|error| format!("check media readiness admission: {error}"))
    }
}

fn run_tape_command(
    report: &DiscoveryReport,
    command: &TapeCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match command {
        TapeCommand::Alerts(args) => run_tape_alerts(report, args, out, err),
        TapeCommand::Init(args) => run_tape_init(report, args, out, err),
        TapeCommand::WaitReady(args) => run_tape_wait_ready(report, args, out, err),
        TapeCommand::Quarantine { .. } => {
            unreachable!("tape quarantine dispatched pre-discovery")
        }
        TapeCommand::Retire(_) => unreachable!("tape retire dispatched pre-discovery"),
    }
}

fn run_tape_alerts(
    report: &DiscoveryReport,
    args: &TapeAlertsArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let config = match remanence_state::load_config(&args.config) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
    };
    let library = match unique_configured_library(report, &config, args.library.as_deref()) {
        Ok(library) => library,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            print_warnings(report, err);
            return ExitCode::from(1);
        }
    };
    let policy = configured_library_policy(&config);
    match run_tape_alerts_hardware(library, &policy, args) {
        Ok(alerts) => {
            print_tape_alerts(library.serial.as_str(), args.bay, &alerts, out);
            print_warnings(report, err);
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            print_setcap_hint_if_error_text_matches(&error, err);
            print_warnings(report, err);
            ExitCode::from(1)
        }
    }
}

#[cfg(target_os = "linux")]
fn run_tape_alerts_hardware(
    library: &Library,
    policy: &StaticAllowlist,
    args: &TapeAlertsArgs,
) -> Result<TapeAlerts, String> {
    let mut handle = open_library_handle(library, policy)
        .map_err(|error| format!("opening library: {error}"))?;
    let mut drive = handle
        .open_drive(args.bay, policy)
        .map_err(|error| format!("open drive 0x{:04x}: {error}", args.bay))?;
    drive
        .read_tape_alerts()
        .map_err(|error| format!("read TapeAlert page: {error}"))
}

#[cfg(not(target_os = "linux"))]
fn run_tape_alerts_hardware(
    _library: &Library,
    _policy: &StaticAllowlist,
    _args: &TapeAlertsArgs,
) -> Result<TapeAlerts, String> {
    Err("tape alerts requires Linux SG_IO drive access in v0.1".to_string())
}

struct TapeWaitReadyResult {
    operation_id: Uuid,
    drive_element: u16,
    barcode: Option<String>,
    readiness: MediaReadiness,
    attempts: u64,
    timed_out: bool,
}

struct TapeWaitReadyFailure {
    message: String,
    exit_code: u8,
}

struct TapeWaitReadyGuidance {
    operator_action: String,
    recommended_next_command: String,
}

struct MediaReadinessPoll {
    readiness: MediaReadiness,
    attempts: u64,
    timed_out: bool,
}

#[cfg(all(test, target_os = "linux"))]
#[allow(dead_code)]
enum MediaReadinessPollEvent<'a> {
    Poll(&'a MediaReadinessPoll),
    Signal(&'a str),
}

#[cfg(target_os = "linux")]
static MEDIA_READINESS_SIGNAL: AtomicI32 = AtomicI32::new(0);

#[cfg(target_os = "linux")]
extern "C" fn media_readiness_signal_handler(signal: libc::c_int) {
    let _ = MEDIA_READINESS_SIGNAL.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
}

#[cfg(target_os = "linux")]
struct MediaReadinessSignalGuard {
    previous_int: libc::sigaction,
    previous_term: libc::sigaction,
}

#[cfg(target_os = "linux")]
impl MediaReadinessSignalGuard {
    fn install() -> Result<Self, String> {
        MEDIA_READINESS_SIGNAL.store(0, Ordering::SeqCst);
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = media_readiness_signal_handler as *const () as usize;
        action.sa_flags = 0;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }

        let mut previous_int: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut previous_term: libc::sigaction = unsafe { std::mem::zeroed() };
        let int_ok = unsafe { libc::sigaction(libc::SIGINT, &action, &mut previous_int) == 0 };
        if !int_ok {
            return Err(format!(
                "install SIGINT handler: {}",
                std::io::Error::last_os_error()
            ));
        }
        let term_ok = unsafe { libc::sigaction(libc::SIGTERM, &action, &mut previous_term) == 0 };
        if !term_ok {
            unsafe {
                libc::sigaction(libc::SIGINT, &previous_int, std::ptr::null_mut());
            }
            return Err(format!(
                "install SIGTERM handler: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            previous_int,
            previous_term,
        })
    }

    fn requested_signal_name(&self) -> Option<&'static str> {
        media_readiness_signal_name(MEDIA_READINESS_SIGNAL.load(Ordering::SeqCst))
    }
}

#[cfg(target_os = "linux")]
impl Drop for MediaReadinessSignalGuard {
    fn drop(&mut self) {
        unsafe {
            libc::sigaction(libc::SIGINT, &self.previous_int, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &self.previous_term, std::ptr::null_mut());
        }
        MEDIA_READINESS_SIGNAL.store(0, Ordering::SeqCst);
    }
}

fn media_readiness_signal_name(signal: i32) -> Option<&'static str> {
    match signal {
        #[cfg(target_os = "linux")]
        libc::SIGINT => Some("SIGINT"),
        #[cfg(target_os = "linux")]
        libc::SIGTERM => Some("SIGTERM"),
        _ => None,
    }
}

fn run_tape_wait_ready(
    report: &DiscoveryReport,
    args: &TapeWaitReadyArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let config = match remanence_state::load_config(&args.config) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
    };
    let library = match unique_configured_library(report, &config, args.library.as_deref()) {
        Ok(library) => library,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            print_warnings(report, err);
            return ExitCode::from(1);
        }
    };
    let policy = configured_library_policy(&config);
    let paths = remanence_state::StatePaths::from_config(&args.config, &config);
    let mut state = match remanence_state::StateHandle::open_with_config(paths, config.clone()) {
        Ok(state) => state,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            print_warnings(report, err);
            return ExitCode::from(1);
        }
    };
    let result = match run_tape_wait_ready_hardware(library, &policy, args, state.catalog_index()) {
        Ok(result) => result,
        Err(failure) => {
            print_tape_wait_ready_failure(library.serial.as_str(), &failure, args.json, out, err);
            print_warnings(report, err);
            return ExitCode::from(failure.exit_code);
        }
    };
    print_tape_wait_ready_result(library.serial.as_str(), &result, args.json, out, err);
    print_warnings(report, err);
    tape_wait_ready_exit_code(&result)
}

#[cfg(target_os = "linux")]
fn run_tape_wait_ready_hardware(
    library: &Library,
    policy: &StaticAllowlist,
    args: &TapeWaitReadyArgs,
    catalog: &mut remanence_state::CatalogIndex,
) -> Result<TapeWaitReadyResult, TapeWaitReadyFailure> {
    let signal_guard =
        MediaReadinessSignalGuard::install().map_err(|message| TapeWaitReadyFailure {
            message,
            exit_code: 1,
        })?;
    let (operation_id, drive_element, barcode) =
        resolve_wait_ready_operation(library, args, catalog).map_err(|message| {
            TapeWaitReadyFailure {
                message,
                exit_code: 50,
            }
        })?;
    let family = barcode
        .as_deref()
        .and_then(remanence_api::lto_generation_from_voltag)
        .map(media_family_for_init_generation)
        .unwrap_or(MediaFamily::Unknown);
    record_media_readiness_signal_if_requested(
        catalog,
        operation_id,
        "before_open_library",
        || signal_guard.requested_signal_name(),
    )
    .map_err(|message| TapeWaitReadyFailure {
        message,
        exit_code: 130,
    })?;
    let mut handle = match open_library_handle(library, policy) {
        Ok(handle) => handle,
        Err(error) => {
            if let Some(signal) = record_media_readiness_signal_or_mechanical_failure(
                catalog,
                operation_id,
                "open_library",
                "transport_unknown",
                None,
                &error.to_string(),
                signal_guard.requested_signal_name(),
            )
            .map_err(|error| TapeWaitReadyFailure {
                message: format!("record media readiness transition: {error}"),
                exit_code: 1,
            })? {
                return Err(TapeWaitReadyFailure {
                    message: signal,
                    exit_code: 130,
                });
            }
            return Err(TapeWaitReadyFailure {
                message: format!("opening library: {error}"),
                exit_code: 40,
            });
        }
    };
    record_media_readiness_signal_if_requested(catalog, operation_id, "after_open_library", || {
        signal_guard.requested_signal_name()
    })
    .map_err(|message| TapeWaitReadyFailure {
        message,
        exit_code: 130,
    })?;
    let mut drive = match handle.open_drive(drive_element, policy) {
        Ok(drive) => drive,
        Err(error) => {
            if let Some(signal) = record_media_readiness_signal_or_mechanical_failure(
                catalog,
                operation_id,
                "open_drive",
                "transport_unknown",
                None,
                &error.to_string(),
                signal_guard.requested_signal_name(),
            )
            .map_err(|error| TapeWaitReadyFailure {
                message: format!("record media readiness transition: {error}"),
                exit_code: 1,
            })? {
                return Err(TapeWaitReadyFailure {
                    message: signal,
                    exit_code: 130,
                });
            }
            return Err(TapeWaitReadyFailure {
                message: format!("open drive 0x{drive_element:04x}: {error}"),
                exit_code: 40,
            });
        }
    };
    record_media_readiness_signal_if_requested(catalog, operation_id, "after_open_drive", || {
        signal_guard.requested_signal_name()
    })
    .map_err(|message| TapeWaitReadyFailure {
        message,
        exit_code: 130,
    })?;
    record_media_readiness_signal_if_requested(
        catalog,
        operation_id,
        "before_wait_ready_tur",
        || signal_guard.requested_signal_name(),
    )
    .map_err(|message| TapeWaitReadyFailure {
        message,
        exit_code: 130,
    })?;
    let initial = drive.probe_media_readiness(family);
    let poll = drive
        .wait_for_media_readiness(
            family,
            Some(initial),
            MediaReadinessWaitOptions {
                wait: args.wait,
                timeout: args.timeout,
                poll_interval: args.poll,
            },
            || signal_guard.requested_signal_name().map(ToOwned::to_owned),
            |event| match event {
                MediaReadinessWaitEvent::Poll(observed) => {
                    let observed = MediaReadinessPoll {
                        readiness: observed.readiness.clone(),
                        attempts: observed.attempts,
                        timed_out: observed.timed_out,
                    };
                    TapeInitStateOps::record_media_readiness_transition(
                        catalog,
                        media_readiness_transition_input(operation_id, "wait_ready_tur", &observed),
                    )
                }
                MediaReadinessWaitEvent::Cancelled(signal) => {
                    record_media_readiness_signal_transition(
                        catalog,
                        operation_id,
                        "wait_ready_tur",
                        signal,
                    )
                }
            },
        )
        .map_err(|message| {
            let message = if message.starts_with("media readiness interrupted by SIG") {
                format!("{message}; recorded aborted_unknown fence")
            } else {
                message
            };
            tape_wait_ready_failure_from_poll_error(message)
        })?;
    record_media_readiness_signal_if_requested(
        catalog,
        operation_id,
        "after_wait_ready_tur",
        || signal_guard.requested_signal_name(),
    )
    .map_err(|message| TapeWaitReadyFailure {
        message,
        exit_code: 130,
    })?;
    Ok(TapeWaitReadyResult {
        operation_id,
        drive_element,
        barcode,
        readiness: poll.readiness,
        attempts: poll.attempts,
        timed_out: poll.timed_out,
    })
}

#[cfg(not(target_os = "linux"))]
fn run_tape_wait_ready_hardware(
    _library: &Library,
    _policy: &StaticAllowlist,
    _args: &TapeWaitReadyArgs,
    _catalog: &mut remanence_state::CatalogIndex,
) -> Result<TapeWaitReadyResult, TapeWaitReadyFailure> {
    Err(TapeWaitReadyFailure {
        message: "tape wait-ready requires Linux SG_IO drive access in v0.1".to_string(),
        exit_code: 1,
    })
}

fn resolve_wait_ready_drive(
    library: &Library,
    args: &TapeWaitReadyArgs,
) -> Result<(u16, Option<String>), String> {
    if let Some(drive_element) = args.drive_element {
        return Ok((drive_element, None));
    }
    let barcode = args
        .barcode
        .as_deref()
        .expect("validated --barcode or --drive-element")
        .trim();
    for bay in &library.drive_bays {
        if bay.loaded_tape.as_deref() == Some(barcode) {
            return Ok((bay.element_address, Some(barcode.to_string())));
        }
    }
    for slot in &library.slots {
        if slot.cartridge.as_deref() == Some(barcode) {
            return Err(format!(
                "barcode {barcode} is in slot 0x{:04x}; wait-ready does not move media",
                slot.element_address
            ));
        }
    }
    Err(format!(
        "barcode {barcode} is not visible in an already-loaded drive in library {}",
        library.serial
    ))
}

fn resolve_wait_ready_operation(
    library: &Library,
    args: &TapeWaitReadyArgs,
    catalog: &mut remanence_state::CatalogIndex,
) -> Result<(Uuid, u16, Option<String>), String> {
    if let Some(operation_id) = args.resume {
        let record = catalog
            .media_readiness_operation(operation_id)
            .map_err(|error| format!("lookup media readiness operation: {error}"))?
            .ok_or_else(|| format!("media readiness operation {operation_id} was not found"))?;
        if record.library_serial != library.serial {
            return Err(format!(
                "media readiness operation {operation_id} belongs to library {}, not {}",
                record.library_serial, library.serial
            ));
        }
        let drive_element = u16::try_from(record.drive_element).map_err(|_| {
            format!(
                "media readiness operation {operation_id} has invalid drive element {}",
                record.drive_element
            )
        })?;
        validate_wait_ready_resume_binding(library, operation_id, drive_element, &record)?;
        return Ok((operation_id, drive_element, record.barcode));
    }

    let (drive_element, barcode) = resolve_wait_ready_drive(library, args)?;
    let conflicts = catalog
        .media_readiness_admission_conflicts(
            library.serial.as_str(),
            Some(drive_element),
            barcode.as_deref(),
            false,
        )
        .map_err(|error| format!("check media readiness admission: {error}"))?;
    if !conflicts.is_empty() {
        return Err(wait_ready_active_conflict_error(&conflicts));
    }
    let operation_id = Uuid::new_v4();
    let drive_bay = library
        .drive_bays
        .iter()
        .find(|bay| bay.element_address == drive_element);
    catalog
        .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
            operation_id,
            run_id: None,
            library_serial: library.serial.clone(),
            changer_sg: Some(library.changer_sg.display().to_string()),
            drive_element,
            drive_sg: drive_bay
                .and_then(|bay| bay.installed.as_ref())
                .and_then(|drive| drive.sg_path.as_ref())
                .map(|path| path.display().to_string()),
            drive_serial: drive_bay
                .and_then(|bay| bay.installed.as_ref())
                .map(|drive| drive.serial.clone()),
            barcode: barcode.clone(),
            source_slot: drive_bay.and_then(|bay| bay.source_slot),
            media_generation: barcode
                .as_deref()
                .and_then(remanence_api::lto_generation_from_voltag)
                .map(|generation| generation.generation_number()),
            phase: "readiness_poll".to_string(),
            state: "planned".to_string(),
            dirty_scope: Some("drive+tape".to_string()),
            deadline_at_utc: args.wait.then(|| deadline_after(args.timeout)).flatten(),
            evidence_path: None,
        })
        .map_err(|error| format!("record media readiness operation: {error}"))?;
    Ok((operation_id, drive_element, barcode))
}

fn wait_ready_active_conflict_error(
    conflicts: &[remanence_state::MediaReadinessOperationRecord],
) -> String {
    let first = conflicts
        .first()
        .map(|record| record.operation_id.as_str())
        .unwrap_or("(unknown)");
    let details = conflicts
        .iter()
        .map(|record| {
            format!(
                "operation={} state={} drive=0x{:04x} barcode={} quarantine={}",
                record.operation_id,
                record.state,
                record.drive_element,
                record.barcode.as_deref().unwrap_or("(unknown)"),
                record.quarantine_id.as_deref().unwrap_or("(none)")
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "active media-readiness operation already fences this target: {details}; use --resume {first} instead of starting a new wait-ready operation"
    )
}

fn validate_wait_ready_resume_binding(
    library: &Library,
    operation_id: Uuid,
    drive_element: u16,
    record: &remanence_state::MediaReadinessOperationRecord,
) -> Result<(), String> {
    const RESUMABLE_STATES: &[&str] = &[
        "planned",
        "pre_ready_loading",
        "media_initializing",
        "becoming_ready",
        "target_busy",
        "unit_attention",
    ];
    if !RESUMABLE_STATES.contains(&record.state.as_str()) {
        return Err(format!(
            "media readiness operation {operation_id} is state {}; use quarantine/recovery flow instead of wait-ready --resume",
            record.state
        ));
    }
    let bay = library
        .drive_bays
        .iter()
        .find(|bay| bay.element_address == drive_element)
        .ok_or_else(|| {
            format!(
                "media readiness operation {operation_id} refers to drive 0x{drive_element:04x}, which is not in selected library {}",
                library.serial
            )
        })?;
    if let Some(expected) = record.drive_serial.as_deref() {
        let actual = bay.installed.as_ref().map(|drive| drive.serial.as_str());
        if actual != Some(expected) {
            return Err(format!(
                "media readiness operation {operation_id} expected drive serial {expected}, selected-library snapshot has {}",
                actual.unwrap_or("(none)")
            ));
        }
    }
    if let Some(expected) = record.barcode.as_deref() {
        if bay.loaded_tape.as_deref() != Some(expected) {
            return Err(format!(
                "media readiness operation {operation_id} expected barcode {expected} in drive 0x{drive_element:04x}, selected-library snapshot has {}",
                bay.loaded_tape.as_deref().unwrap_or("(none)")
            ));
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
fn poll_drive_media_readiness(
    drive: &mut DriveHandle,
    family: MediaFamily,
    wait: bool,
    timeout: StdDuration,
    poll: StdDuration,
    mut signal: impl FnMut() -> Option<&'static str>,
    mut record: impl for<'a> FnMut(MediaReadinessPollEvent<'a>) -> Result<(), String>,
) -> Result<MediaReadinessPoll, String> {
    const SIGNAL_SLEEP_SLICE: StdDuration = StdDuration::from_millis(250);
    let started = StdInstant::now();
    let mut attempts = 0_u64;
    let mut unit_attention_seen = BTreeSet::<(u8, u8)>::new();
    loop {
        if let Some(signal) = signal() {
            record(MediaReadinessPollEvent::Signal(signal))?;
            return Err(format!(
                "media readiness interrupted by {signal}; recorded aborted_unknown fence"
            ));
        }
        attempts = attempts.saturating_add(1);
        let mut readiness = drive.probe_media_readiness(family);
        terminalize_repeated_unit_attention(&mut readiness, &mut unit_attention_seen);
        let terminal = readiness.is_ready() || !wait || !readiness.is_retryable_wait();
        let elapsed = started.elapsed();
        let timed_out = !terminal && elapsed >= timeout;
        let current = MediaReadinessPoll {
            readiness,
            attempts,
            timed_out,
        };
        record(MediaReadinessPollEvent::Poll(&current))?;
        if let Some(signal) = signal() {
            record(MediaReadinessPollEvent::Signal(signal))?;
            return Err(format!(
                "media readiness interrupted by {signal}; recorded aborted_unknown fence"
            ));
        }
        if terminal || timed_out {
            return Ok(current);
        }
        let sleep_for = media_conditioning_poll_interval(elapsed, poll);
        let mut remaining = std::cmp::min(sleep_for, timeout - elapsed);
        while remaining > StdDuration::ZERO {
            if let Some(signal) = signal() {
                record(MediaReadinessPollEvent::Signal(signal))?;
                return Err(format!(
                    "media readiness interrupted by {signal}; recorded aborted_unknown fence"
                ));
            }
            let chunk = std::cmp::min(SIGNAL_SLEEP_SLICE, remaining);
            media_readiness_sleep(chunk);
            remaining = remaining.saturating_sub(chunk);
        }
    }
}

#[cfg(target_os = "linux")]
fn terminalize_repeated_unit_attention(
    readiness: &mut MediaReadiness,
    seen: &mut BTreeSet<(u8, u8)>,
) {
    let repeated = match readiness {
        MediaReadiness::UnitAttention { asc, ascq } => {
            let key = (*asc, *ascq);
            (!seen.insert(key)).then_some(key)
        }
        _ => None,
    };
    if let Some((asc, ascq)) = repeated {
        *readiness = MediaReadiness::RepeatedUnitAttention { asc, ascq };
    }
}

#[cfg(target_os = "linux")]
fn media_conditioning_poll_interval(elapsed: StdDuration, steady_poll: StdDuration) -> StdDuration {
    remanence_library::media_readiness_poll_interval(elapsed, steady_poll)
}

#[cfg(all(target_os = "linux", not(test)))]
fn media_readiness_sleep(duration: StdDuration) {
    std::thread::sleep(duration);
}

#[cfg(all(target_os = "linux", test))]
fn media_readiness_sleep(_duration: StdDuration) {}

#[cfg(target_os = "linux")]
fn poll_already_observed_media_readiness(
    readiness: MediaReadiness,
    phase: &str,
    operation_id: Uuid,
    state: &mut impl TapeInitStateOps,
) -> Result<MediaReadinessPoll, String> {
    let poll = MediaReadinessPoll {
        readiness,
        attempts: 1,
        timed_out: false,
    };
    state.record_media_readiness_transition(media_readiness_transition_input(
        operation_id,
        phase,
        &poll,
    ))?;
    Ok(poll)
}

#[cfg(target_os = "linux")]
struct MediaReadinessInitialProbeInput {
    initial_poll: MediaReadinessPoll,
    wait: bool,
    timeout: StdDuration,
    poll_interval: StdDuration,
    conditional_load_on_no_medium: bool,
}

#[cfg(target_os = "linux")]
fn poll_media_readiness_after_initial_probe<S: TapeInitStateOps>(
    drive: &mut DriveHandle,
    family: MediaFamily,
    operation_id: Uuid,
    state: &mut S,
    drive_element: u16,
    input: MediaReadinessInitialProbeInput,
    mut signal: impl FnMut() -> Option<&'static str>,
) -> Result<MediaReadinessPoll, String> {
    const SIGNAL_SLEEP_SLICE: StdDuration = StdDuration::from_millis(250);
    let MediaReadinessInitialProbeInput {
        initial_poll,
        wait,
        timeout,
        poll_interval,
        conditional_load_on_no_medium,
    } = input;

    let started = StdInstant::now();
    let mut attempts = initial_poll.attempts;
    let mut unit_attention_seen = BTreeSet::<(u8, u8)>::new();
    if let MediaReadiness::UnitAttention { asc, ascq } = &initial_poll.readiness {
        unit_attention_seen.insert((*asc, *ascq));
    }
    let mut current = Some(initial_poll);
    let mut conditional_load_sent = false;

    loop {
        let poll = if let Some(poll) = current.take() {
            poll
        } else {
            if let Some(signal_name) = signal() {
                record_media_readiness_signal_transition(
                    state,
                    operation_id,
                    "readiness_poll",
                    signal_name,
                )?;
                return Err(format!(
                    "media readiness interrupted by {signal_name}; recorded aborted_unknown fence"
                ));
            }
            attempts = attempts.saturating_add(1);
            let mut readiness = drive.probe_media_readiness(family);
            terminalize_repeated_unit_attention(&mut readiness, &mut unit_attention_seen);
            let can_conditionally_load = wait
                && !conditional_load_sent
                && readiness_requires_conditional_load(&readiness, conditional_load_on_no_medium);
            let terminal = readiness.is_ready()
                || !wait
                || !(readiness.is_retryable_wait() || can_conditionally_load);
            let elapsed = started.elapsed();
            let timed_out = !terminal && elapsed >= timeout;
            let poll = MediaReadinessPoll {
                readiness,
                attempts,
                timed_out,
            };
            state.record_media_readiness_transition(media_readiness_transition_input(
                operation_id,
                "readiness_poll",
                &poll,
            ))?;
            poll
        };

        if let Some(signal_name) = signal() {
            record_media_readiness_signal_transition(
                state,
                operation_id,
                "readiness_poll",
                signal_name,
            )?;
            return Err(format!(
                "media readiness interrupted by {signal_name}; recorded aborted_unknown fence"
            ));
        }

        if wait
            && !conditional_load_sent
            && readiness_requires_conditional_load(&poll.readiness, conditional_load_on_no_medium)
        {
            conditional_load_sent = true;
            record_media_readiness_signal_if_requested(
                state,
                operation_id,
                "before_conditional_immediate_load",
                &mut signal,
            )?;
            state.record_media_readiness_transition(media_readiness_mechanical_transition(
                operation_id,
                "conditional_immediate_load",
                "pre_ready_loading",
                Some(0x1b),
                None,
            ))?;
            media_readiness_sleep(CONDITIONAL_LOAD_SETTLE);
            if let Err(error) = drive.load_immediate() {
                if let Some(signal_name) = record_media_readiness_signal_or_mechanical_failure(
                    state,
                    operation_id,
                    "conditional_immediate_load",
                    "transport_unknown",
                    Some(0x1b),
                    &error.to_string(),
                    signal(),
                )? {
                    return Err(signal_name);
                }
                return Err(format!(
                    "media_readiness_state=transport_unknown media_readiness_exit_code=40: immediate drive load 0x{drive_element:04x}: {error}"
                ));
            }
            unit_attention_seen.clear();
            record_media_readiness_signal_if_requested(
                state,
                operation_id,
                "after_conditional_immediate_load",
                &mut signal,
            )?;
            continue;
        }

        let terminal = poll.readiness.is_ready()
            || !wait
            || !poll.readiness.is_retryable_wait()
            || poll.timed_out;
        if terminal {
            return Ok(poll);
        }

        let elapsed = started.elapsed();
        let sleep_for = media_conditioning_poll_interval(elapsed, poll_interval);
        let mut remaining = std::cmp::min(sleep_for, timeout.saturating_sub(elapsed));
        while remaining > StdDuration::ZERO {
            if let Some(signal_name) = signal() {
                record_media_readiness_signal_transition(
                    state,
                    operation_id,
                    "readiness_poll",
                    signal_name,
                )?;
                return Err(format!(
                    "media readiness interrupted by {signal_name}; recorded aborted_unknown fence"
                ));
            }
            let chunk = std::cmp::min(SIGNAL_SLEEP_SLICE, remaining);
            media_readiness_sleep(chunk);
            remaining = remaining.saturating_sub(chunk);
        }
    }
}

#[cfg(target_os = "linux")]
fn poll_tape_init_readiness_after_initial_probe<S: TapeInitStateOps>(
    drive: &mut DriveHandle,
    family: MediaFamily,
    operation_id: Uuid,
    state: &mut S,
    drive_element: u16,
    input: MediaReadinessInitialProbeInput,
    signal: impl FnMut() -> Option<&'static str>,
) -> Result<MediaReadinessPoll, String> {
    poll_media_readiness_after_initial_probe(
        drive,
        family,
        operation_id,
        state,
        drive_element,
        input,
        signal,
    )
}

fn deadline_after(timeout: StdDuration) -> Option<String> {
    let seconds = i64::try_from(timeout.as_secs()).ok()?;
    OffsetDateTime::now_utc()
        .checked_add(Duration::seconds(seconds))
        .and_then(|deadline| deadline.format(&Rfc3339).ok())
}

fn media_readiness_transition_input(
    operation_id: Uuid,
    phase: &str,
    poll: &MediaReadinessPoll,
) -> remanence_state::MediaReadinessTransitionInput {
    let (sense_key, asc, ascq, target_status, transport_class, last_error_json, sense_raw) =
        media_readiness_evidence_fields(&poll.readiness);
    let state = media_readiness_durable_state(&poll.readiness, poll.timed_out).to_string();
    let quarantine_id = media_readiness_state_requires_release(state.as_str())
        .then(|| media_readiness_quarantine_id_for_operation(operation_id));
    remanence_state::MediaReadinessTransitionInput {
        operation_id,
        phase: Some(phase.to_string()),
        state,
        dirty_scope: Some(if poll.readiness.is_ready() {
            "none".to_string()
        } else {
            "drive+tape".to_string()
        }),
        last_cdb_opcode: Some(0x00),
        last_sense_raw: sense_raw,
        last_sense_key: sense_key,
        last_asc: asc,
        last_ascq: ascq,
        last_host_status: None,
        last_driver_status: None,
        target_status,
        transport_class,
        cancel_source: None,
        signal: None,
        evidence_path: None,
        last_error_json,
        quarantine_id,
    }
}

fn media_readiness_mechanical_transition(
    operation_id: Uuid,
    phase: &str,
    state: &str,
    cdb_opcode: Option<u8>,
    error: Option<String>,
) -> remanence_state::MediaReadinessTransitionInput {
    remanence_state::MediaReadinessTransitionInput {
        operation_id,
        phase: Some(phase.to_string()),
        state: state.to_string(),
        dirty_scope: Some("drive+tape".to_string()),
        last_cdb_opcode: cdb_opcode,
        last_sense_raw: None,
        last_sense_key: None,
        last_asc: None,
        last_ascq: None,
        last_host_status: None,
        last_driver_status: None,
        target_status: None,
        transport_class: (state == "transport_unknown").then(|| "unknown".to_string()),
        cancel_source: None,
        signal: None,
        evidence_path: None,
        last_error_json: error.map(|detail| json!({ "detail": detail }).to_string()),
        quarantine_id: media_readiness_state_requires_release(state)
            .then(|| media_readiness_quarantine_id_for_operation(operation_id)),
    }
}

fn media_readiness_signal_transition(
    operation_id: Uuid,
    phase: &str,
    signal: &str,
) -> remanence_state::MediaReadinessTransitionInput {
    remanence_state::MediaReadinessTransitionInput {
        operation_id,
        phase: Some(phase.to_string()),
        state: "aborted_unknown".to_string(),
        dirty_scope: Some("drive+tape".to_string()),
        last_cdb_opcode: None,
        last_sense_raw: None,
        last_sense_key: None,
        last_asc: None,
        last_ascq: None,
        last_host_status: None,
        last_driver_status: None,
        target_status: None,
        transport_class: Some("unknown".to_string()),
        cancel_source: Some("signal".to_string()),
        signal: Some(signal.to_string()),
        evidence_path: None,
        last_error_json: Some(
            json!({
                "detail": format!("media readiness interrupted by {signal}"),
                "action": "startup reconciliation or operator release required before retry"
            })
            .to_string(),
        ),
        quarantine_id: Some(media_readiness_quarantine_id_for_operation(operation_id)),
    }
}

fn record_media_readiness_signal_transition<S: TapeInitStateOps>(
    state: &mut S,
    operation_id: Uuid,
    phase: &str,
    signal: &str,
) -> Result<(), String> {
    state.record_media_readiness_transition(media_readiness_signal_transition(
        operation_id,
        phase,
        signal,
    ))
}

fn record_media_readiness_signal_if_requested<S, F>(
    state: &mut S,
    operation_id: Uuid,
    phase: &str,
    mut signal: F,
) -> Result<(), String>
where
    S: TapeInitStateOps,
    F: FnMut() -> Option<&'static str>,
{
    let Some(signal) = signal() else {
        return Ok(());
    };
    record_media_readiness_signal_transition(state, operation_id, phase, signal)?;
    Err(media_readiness_signal_abort_message(signal))
}

fn media_readiness_signal_abort_message(signal: &str) -> String {
    format!("media readiness interrupted by {signal}; recorded aborted_unknown fence")
}

fn media_readiness_error_is_signal_abort(message: &str) -> bool {
    message.contains("media readiness interrupted by SIG")
        && message.contains("recorded aborted_unknown fence")
}

fn tape_wait_ready_failure_from_poll_error(message: String) -> TapeWaitReadyFailure {
    let exit_code = if let Some(code) = media_readiness_failure_exit_code(&message) {
        code
    } else if media_readiness_error_is_signal_abort(&message) {
        130
    } else {
        1
    };
    TapeWaitReadyFailure { message, exit_code }
}

fn media_readiness_command_failure_state(error: &str) -> &'static str {
    let lower = error.to_ascii_lowercase();
    if lower.contains("transport error")
        || lower.contains("completion unknown")
        || lower.contains("host_status")
        || lower.contains("did_")
        || lower.contains("task aborted")
        || lower.contains("resetting scsi")
        || lower.contains("timed out")
        || lower.contains("timeout")
    {
        "transport_unknown"
    } else {
        "terminal_error"
    }
}

fn record_media_readiness_command_failure<S: TapeInitStateOps>(
    state: &mut S,
    operation_id: Uuid,
    phase: &str,
    cdb_opcode: Option<u8>,
    error: &str,
) -> Result<(), String> {
    state.record_media_readiness_transition(media_readiness_mechanical_transition(
        operation_id,
        phase,
        media_readiness_command_failure_state(error),
        cdb_opcode,
        Some(error.to_string()),
    ))
}

fn record_media_readiness_signal_or_mechanical_failure<S: TapeInitStateOps>(
    state: &mut S,
    operation_id: Uuid,
    phase: &str,
    mechanical_state: &str,
    cdb_opcode: Option<u8>,
    error: &str,
    signal: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(signal) = signal {
        record_media_readiness_signal_transition(state, operation_id, phase, signal)?;
        return Ok(Some(media_readiness_signal_abort_message(signal)));
    }
    state.record_media_readiness_transition(media_readiness_mechanical_transition(
        operation_id,
        phase,
        mechanical_state,
        cdb_opcode,
        Some(error.to_string()),
    ))?;
    Ok(None)
}

fn record_media_readiness_signal_or_command_failure<S: TapeInitStateOps>(
    state: &mut S,
    operation_id: Uuid,
    phase: &str,
    cdb_opcode: Option<u8>,
    error: &str,
    signal: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(signal) = signal {
        record_media_readiness_signal_transition(state, operation_id, phase, signal)?;
        return Ok(Some(media_readiness_signal_abort_message(signal)));
    }
    record_media_readiness_command_failure(state, operation_id, phase, cdb_opcode, error)?;
    Ok(None)
}

fn media_readiness_release_transition(
    operation_id: Uuid,
    ack: &str,
) -> remanence_state::MediaReadinessTransitionInput {
    remanence_state::MediaReadinessTransitionInput {
        operation_id,
        phase: Some("quarantine_release".to_string()),
        state: "released".to_string(),
        dirty_scope: Some("none".to_string()),
        last_cdb_opcode: None,
        last_sense_raw: None,
        last_sense_key: None,
        last_asc: None,
        last_ascq: None,
        last_host_status: None,
        last_driver_status: None,
        target_status: None,
        transport_class: None,
        cancel_source: Some("operator".to_string()),
        signal: None,
        evidence_path: None,
        last_error_json: Some(json!({ "ack": ack }).to_string()),
        quarantine_id: Some(media_readiness_quarantine_id_for_operation(operation_id)),
    }
}

fn media_readiness_state_requires_release(state: &str) -> bool {
    matches!(
        state,
        "aborted_unknown"
            | "timeout_unknown"
            | "transport_unknown"
            | "terminal_error"
            | "reservation_conflict"
    )
}

fn media_readiness_quarantine_id_for_operation(operation_id: Uuid) -> String {
    format!("mrq-{operation_id}")
}

type MediaReadinessEvidenceFields = (
    Option<u8>,
    Option<u8>,
    Option<u8>,
    Option<u8>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn media_readiness_evidence_fields(readiness: &MediaReadiness) -> MediaReadinessEvidenceFields {
    match readiness {
        MediaReadiness::Ready => (None, None, None, None, None, None, None),
        MediaReadiness::BecomingReady { ascq, .. } => {
            (Some(0x02), Some(0x04), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::NoMedium { ascq } => {
            (Some(0x02), Some(0x3a), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::UnitAttention { asc, ascq } => {
            (Some(0x06), Some(*asc), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::RepeatedUnitAttention { asc, ascq } => (
            Some(0x06),
            Some(*asc),
            Some(*ascq),
            None,
            None,
            Some(json!({ "action": "repeated_unit_attention" }).to_string()),
            None,
        ),
        MediaReadiness::TerminalNotReady { ascq, action } => (
            Some(0x02),
            Some(0x04),
            Some(*ascq),
            None,
            None,
            Some(json!({ "action": action }).to_string()),
            None,
        ),
        MediaReadiness::CheckCondition { key, asc, ascq } => {
            (Some(*key), Some(*asc), Some(*ascq), None, None, None, None)
        }
        MediaReadiness::UndecodedCheckCondition { sense } => (
            None,
            None,
            None,
            None,
            None,
            Some(json!({ "error": "undecoded_check_condition" }).to_string()),
            Some(bytes_to_hex(sense)),
        ),
        MediaReadiness::TargetBusy { status } | MediaReadiness::UnexpectedStatus { status } => {
            (None, None, None, Some(*status), None, None, None)
        }
        MediaReadiness::ReservationConflict => (None, None, None, Some(0x18), None, None, None),
        MediaReadiness::TaskAborted => (None, None, None, Some(0x40), None, None, None),
        MediaReadiness::TransportUnknown { detail } => (
            None,
            None,
            None,
            None,
            Some("unknown".to_string()),
            Some(json!({ "detail": detail }).to_string()),
            None,
        ),
        MediaReadiness::InvalidRequest { detail } => (
            None,
            None,
            None,
            None,
            None,
            Some(json!({ "detail": detail }).to_string()),
            None,
        ),
    }
}

fn media_readiness_durable_state(readiness: &MediaReadiness, timed_out: bool) -> &'static str {
    if timed_out {
        return "timeout_unknown";
    }
    match readiness {
        MediaReadiness::Ready => "ready",
        MediaReadiness::BecomingReady {
            media_initializing: true,
            ..
        } => "media_initializing",
        MediaReadiness::BecomingReady { .. } => "becoming_ready",
        MediaReadiness::NoMedium { .. } => "terminal_error",
        MediaReadiness::UnitAttention { .. } => "unit_attention",
        MediaReadiness::RepeatedUnitAttention { .. } => "terminal_error",
        MediaReadiness::TerminalNotReady { .. } => "terminal_error",
        MediaReadiness::CheckCondition { .. } => "terminal_error",
        MediaReadiness::UndecodedCheckCondition { .. } => "terminal_error",
        MediaReadiness::TargetBusy { .. } => "target_busy",
        MediaReadiness::ReservationConflict => "reservation_conflict",
        MediaReadiness::TaskAborted => "terminal_error",
        MediaReadiness::UnexpectedStatus { .. } => "terminal_error",
        MediaReadiness::TransportUnknown { .. } => "transport_unknown",
        MediaReadiness::InvalidRequest { .. } => "terminal_error",
    }
}

fn readiness_requires_conditional_load(
    readiness: &MediaReadiness,
    conditional_load_on_no_medium: bool,
) -> bool {
    matches!(readiness, MediaReadiness::BecomingReady { ascq: 0x02, .. })
        || (conditional_load_on_no_medium && matches!(readiness, MediaReadiness::NoMedium { .. }))
}

fn tape_init_readiness_exit_code(poll: &MediaReadinessPoll) -> u8 {
    if poll.timed_out {
        20
    } else {
        u8::try_from(poll.readiness.design_exit_code()).unwrap_or(1)
    }
}

fn media_readiness_failure_exit_code(error: &str) -> Option<u8> {
    const MARKER: &str = "media_readiness_exit_code=";
    let marker_at = error.find(MARKER)?;
    let value_start = marker_at + MARKER.len();
    let value = error[value_start..]
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    value.parse().ok()
}

fn tape_init_failure_exit_code(error: &str) -> Option<u8> {
    media_readiness_failure_exit_code(error)
}

fn print_tape_wait_ready_result(
    library_serial: &str,
    result: &TapeWaitReadyResult,
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) {
    let summary = if result.timed_out {
        format!(
            "timed out waiting for media readiness; last state: {}",
            describe_media_readiness(&result.readiness)
        )
    } else {
        describe_media_readiness(&result.readiness)
    };
    let guidance = tape_wait_ready_result_guidance(library_serial, result);
    if json_output {
        let payload = json!({
            "schema": "rem.tape.wait_ready.v1",
            "operation_id": result.operation_id.to_string(),
            "library_serial": library_serial,
            "drive_element": format!("0x{:04x}", result.drive_element),
            "barcode": result.barcode,
            "state": media_readiness_state_name(&result.readiness, result.timed_out),
            "ready": result.readiness.is_ready(),
            "retryable": result.readiness.is_retryable_wait(),
            "timed_out": result.timed_out,
            "attempts": result.attempts,
            "exit_code": tape_wait_ready_exit_code_u8(result),
            "summary": summary,
            "operator_action": guidance.operator_action,
            "recommended_next_command": guidance.recommended_next_command,
        });
        let _ = serde_json::to_writer_pretty(&mut *out, &payload);
        let _ = writeln!(out);
        return;
    }
    let barcode = result
        .barcode
        .as_deref()
        .map(|value| format!(" barcode={value}"))
        .unwrap_or_default();
    let status = if result.readiness.is_ready() {
        "ready"
    } else if result.timed_out {
        "timeout"
    } else {
        "not-ready"
    };
    let _ = writeln!(
        out,
        "{status} operation_id={} library={library_serial} drive=0x{:04x}{barcode} attempts={} {summary}",
        result.operation_id, result.drive_element, result.attempts
    );
    if tape_wait_ready_exit_code_u8(result) != 0 {
        let _ = writeln!(err, "operator_action: {}", guidance.operator_action);
        let _ = writeln!(
            err,
            "recommended_next_command: {}",
            guidance.recommended_next_command
        );
    }
}

fn print_tape_wait_ready_failure(
    library_serial: &str,
    failure: &TapeWaitReadyFailure,
    json_output: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) {
    let guidance = tape_wait_ready_failure_guidance(library_serial, failure);
    if json_output {
        let payload = json!({
            "schema": "rem.tape.wait_ready.v1",
            "library_serial": library_serial,
            "state": tape_wait_ready_failure_state(failure.exit_code),
            "ready": false,
            "retryable": false,
            "timed_out": failure.exit_code == 20,
            "exit_code": failure.exit_code,
            "summary": failure.message.as_str(),
            "operator_action": guidance.operator_action,
            "recommended_next_command": guidance.recommended_next_command,
        });
        let _ = serde_json::to_writer_pretty(&mut *out, &payload);
        let _ = writeln!(out);
        return;
    }
    let _ = writeln!(err, "error: {}", failure.message);
    print_setcap_hint_if_error_text_matches(&failure.message, err);
    if matches!(failure.exit_code, 20 | 30 | 40 | 50 | 130) {
        let _ = writeln!(err, "operator_action: {}", guidance.operator_action);
        let _ = writeln!(
            err,
            "recommended_next_command: {}",
            guidance.recommended_next_command
        );
    }
}

fn tape_wait_ready_result_guidance(
    library_serial: &str,
    result: &TapeWaitReadyResult,
) -> TapeWaitReadyGuidance {
    let operation_id = result.operation_id;
    let quarantine_id = media_readiness_quarantine_id_for_operation(operation_id);
    match tape_wait_ready_exit_code_u8(result) {
        0 => TapeWaitReadyGuidance {
            operator_action: "media is ready; normal rem commands may proceed".to_string(),
            recommended_next_command: "none".to_string(),
        },
        10 => TapeWaitReadyGuidance {
            operator_action: "leave the cartridge in the drive; do not move, unload, force, or clobber; resume the readiness wait later".to_string(),
            recommended_next_command: format!(
                "rem tape wait-ready --library {library_serial} --resume {operation_id} --wait --json"
            ),
        },
        20 => TapeWaitReadyGuidance {
            operator_action: "timeout_unknown: keep the drive and tape fenced; collect RCA evidence and release only after settled inventory".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine show {quarantine_id} --json"
            ),
        },
        30 => TapeWaitReadyGuidance {
            operator_action: "terminal_error: stop; inspect the readiness quarantine and hardware/media evidence before any retry".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine show {quarantine_id} --json"
            ),
        },
        40 => TapeWaitReadyGuidance {
            operator_action: "transport_unknown: stop; keep media fenced, capture kernel/SCSI evidence, and reconcile inventory before retrying".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine show {quarantine_id} --json"
            ),
        },
        50 => TapeWaitReadyGuidance {
            operator_action: "ownership/refused: verify selected library, barcode binding, allowlist, and other owners before retrying".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine show {quarantine_id} --json"
            ),
        },
        130 => TapeWaitReadyGuidance {
            operator_action: "aborted_unknown: leave the cartridge in place; inspect or resume the fenced readiness operation before any move/unload".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine show {quarantine_id} --json"
            ),
        },
        _ => TapeWaitReadyGuidance {
            operator_action: "stop and inspect the captured wait-ready error before retrying".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine list --library {library_serial} --json"
            ),
        },
    }
}

fn tape_wait_ready_failure_guidance(
    library_serial: &str,
    failure: &TapeWaitReadyFailure,
) -> TapeWaitReadyGuidance {
    match failure.exit_code {
        20 => TapeWaitReadyGuidance {
            operator_action: "timeout_unknown: keep the drive and tape fenced; collect RCA evidence and release only after settled inventory".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine list --library {library_serial} --json"
            ),
        },
        30 => TapeWaitReadyGuidance {
            operator_action: "terminal_error: stop; inspect readiness and hardware/media evidence before any retry".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine list --library {library_serial} --json"
            ),
        },
        40 => TapeWaitReadyGuidance {
            operator_action: "transport_unknown: stop; keep media fenced, capture kernel/SCSI evidence, and reconcile inventory before retrying".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine list --library {library_serial} --json"
            ),
        },
        50 => TapeWaitReadyGuidance {
            operator_action: "ownership/refused: verify selected library, loaded barcode, allowlist, and active owner; do not search or move media from another library".to_string(),
            recommended_next_command: format!("rem library {library_serial} --slots"),
        },
        130 => TapeWaitReadyGuidance {
            operator_action: "aborted_unknown: leave the cartridge in place; inspect the fenced readiness operation before any move/unload".to_string(),
            recommended_next_command: format!(
                "rem tape quarantine list --library {library_serial} --json"
            ),
        },
        _ => TapeWaitReadyGuidance {
            operator_action: "stop and inspect the captured wait-ready error before retrying".to_string(),
            recommended_next_command: format!("rem library {library_serial} --slots"),
        },
    }
}

fn tape_wait_ready_failure_state(exit_code: u8) -> &'static str {
    match exit_code {
        20 => "timeout_unknown",
        30 => "terminal_error",
        40 => "transport_unknown",
        50 => "ownership_refused",
        130 => "aborted_unknown",
        _ => "command_error",
    }
}

fn media_readiness_state_name(readiness: &MediaReadiness, timed_out: bool) -> &'static str {
    if timed_out {
        return "timeout_unknown";
    }
    match readiness {
        MediaReadiness::Ready => "ready",
        MediaReadiness::BecomingReady {
            media_initializing: true,
            ..
        } => "media_initializing",
        MediaReadiness::BecomingReady { .. } => "becoming_ready",
        MediaReadiness::NoMedium { .. } => "no_medium",
        MediaReadiness::UnitAttention { .. } => "unit_attention",
        MediaReadiness::RepeatedUnitAttention { .. } => "repeated_unit_attention",
        MediaReadiness::TerminalNotReady { .. } => "terminal_not_ready",
        MediaReadiness::CheckCondition { .. } => "check_condition",
        MediaReadiness::UndecodedCheckCondition { .. } => "undecoded_check_condition",
        MediaReadiness::TargetBusy { .. } => "target_busy",
        MediaReadiness::ReservationConflict => "reservation_conflict",
        MediaReadiness::TaskAborted => "task_aborted",
        MediaReadiness::UnexpectedStatus { .. } => "unexpected_status",
        MediaReadiness::TransportUnknown { .. } => "transport_unknown",
        MediaReadiness::InvalidRequest { .. } => "invalid_request",
    }
}

fn tape_wait_ready_exit_code(result: &TapeWaitReadyResult) -> ExitCode {
    ExitCode::from(tape_wait_ready_exit_code_u8(result))
}

fn tape_wait_ready_exit_code_u8(result: &TapeWaitReadyResult) -> u8 {
    if result.timed_out {
        20
    } else {
        u8::try_from(result.readiness.design_exit_code()).unwrap_or(1)
    }
}

fn print_tape_alerts(library_serial: &str, bay: u16, alerts: &TapeAlerts, out: &mut dyn Write) {
    let active = alerts
        .active()
        .iter()
        .map(|flag| {
            json!({
                "flag": flag,
                "name": tape_alert_flag_name(*flag),
            })
        })
        .collect::<Vec<_>>();
    let envelope = json!({
        "schema": "rem.tape.alerts.v1",
        "library_serial": library_serial,
        "bay_element": bay,
        "active": active,
    });
    if let Ok(line) = serde_json::to_string(&envelope) {
        let _ = writeln!(out, "{line}");
    }
}

fn run_tape_init(
    report: &DiscoveryReport,
    args: &TapeInitArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let target = match parse_tape_init_target(args.target.as_str()) {
        Ok(target) => target,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
    };
    let config = match remanence_state::load_config(&args.config) {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
    };
    let paths = remanence_state::StatePaths::from_config(&args.config, &config);
    let candidates =
        match resolve_tape_init_candidates(report, &config, args.library.as_deref(), &target) {
            Ok(candidates) => candidates,
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                print_warnings(report, err);
                return ExitCode::from(1);
            }
        };
    let policy = configured_library_policy(&config);

    if args.dry_run {
        let catalog = match remanence_state::CatalogIndex::open_read_only(&paths.sqlite_path) {
            Ok(catalog) => catalog,
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                print_warnings(report, err);
                return ExitCode::from(1);
            }
        };
        let mut state = DryRunTapeInitState::new(catalog);
        let ctx = TapeInitRunContext {
            report,
            config: &config,
            policy: &policy,
            args,
        };
        return run_tape_init_candidates(&mut state, candidates, &ctx, out, err);
    }

    let mut state = match remanence_state::StateHandle::open_with_config(paths, config.clone()) {
        Ok(state) => state,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            print_warnings(report, err);
            return ExitCode::from(1);
        }
    };
    if let Err(error) =
        remanence_api::reconcile_media_readiness_on_startup(state.catalog_index(), report, &policy)
    {
        let _ = writeln!(err, "error: startup media-readiness reconcile: {error}");
        print_warnings(report, err);
        return ExitCode::from(1);
    }
    let ctx = TapeInitRunContext {
        report,
        config: &config,
        policy: &policy,
        args,
    };
    run_tape_init_candidates(&mut state, candidates, &ctx, out, err)
}

fn run_tape_init_candidates<S: TapeInitStateOps>(
    state: &mut S,
    candidates: Vec<TapeInitCandidate>,
    ctx: &TapeInitRunContext<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let mut successes = 0usize;
    let mut failures = 0usize;
    let mut readiness_failure_exit_code: Option<u8> = None;
    let mut non_readiness_failures = 0usize;
    let count = candidates.len();
    for candidate in candidates {
        match run_one_tape_init(state, ctx, candidate.clone(), err) {
            Ok(result) => {
                print_tape_init_result(&result, out);
                if result.success {
                    successes += 1;
                } else {
                    failures += 1;
                    non_readiness_failures += 1;
                }
            }
            Err(error) => {
                let _ = writeln!(err, "tape init {}: {error}", candidate.label());
                failures += 1;
                if let Some(exit_code) = tape_init_failure_exit_code(&error) {
                    readiness_failure_exit_code.get_or_insert(exit_code);
                } else {
                    non_readiness_failures += 1;
                }
            }
        }
    }
    if count > 1 {
        let _ = writeln!(out, "summary: {successes} ok, {failures} failed");
    }
    print_warnings(ctx.report, err);
    if failures == 0 {
        ExitCode::SUCCESS
    } else if non_readiness_failures == 0 {
        ExitCode::from(readiness_failure_exit_code.unwrap_or(1))
    } else if ctx.args.dry_run {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn resolve_tape_init_candidates(
    report: &DiscoveryReport,
    config: &remanence_state::RemConfig,
    requested_library: Option<&str>,
    target: &TapeInitTarget,
) -> Result<Vec<TapeInitCandidate>, String> {
    match target {
        TapeInitTarget::Voltag(voltag) => {
            let mut hits = Vec::new();
            for library in configured_report_libraries(report, config, requested_library)? {
                for slot in &library.slots {
                    if slot.full && slot.cartridge.as_deref() == Some(voltag.as_str()) {
                        hits.push(TapeInitCandidate {
                            library_serial: library.serial.clone(),
                            element_address: slot.element_address,
                            location: TapeInitLocation::Slot,
                            voltag: Some(voltag.clone()),
                        });
                    }
                }
                for bay in &library.drive_bays {
                    if bay.loaded && bay.loaded_tape.as_deref() == Some(voltag.as_str()) {
                        hits.push(TapeInitCandidate {
                            library_serial: library.serial.clone(),
                            element_address: bay.element_address,
                            location: TapeInitLocation::Drive,
                            voltag: Some(voltag.clone()),
                        });
                    }
                }
            }
            unique_or_error(hits, format!("barcode {voltag:?}"))
        }
        TapeInitTarget::Element(element) => {
            let mut hits = Vec::new();
            for library in configured_report_libraries(report, config, requested_library)? {
                if let Some(slot) = library
                    .slots
                    .iter()
                    .find(|slot| slot.element_address == *element)
                {
                    hits.push(TapeInitCandidate {
                        library_serial: library.serial.clone(),
                        element_address: *element,
                        location: TapeInitLocation::Slot,
                        voltag: slot.cartridge.clone(),
                    });
                }
                if let Some(bay) = library
                    .drive_bays
                    .iter()
                    .find(|bay| bay.element_address == *element)
                {
                    hits.push(TapeInitCandidate {
                        library_serial: library.serial.clone(),
                        element_address: *element,
                        location: TapeInitLocation::Drive,
                        voltag: bay.loaded_tape.clone(),
                    });
                }
            }
            unique_or_error(hits, format!("element 0x{element:04x}"))
        }
        TapeInitTarget::SlotRange { start, end } => {
            let library = unique_range_library(report, config, requested_library, *start, *end)?;
            let mut candidates = Vec::new();
            for element in *start..=*end {
                let slot = library.slots.iter().find(|slot| slot.element_address == element)
                    .ok_or_else(|| {
                        format!(
                            "slot range includes 0x{element:04x}, which is not a storage slot in library {}",
                            library.serial
                        )
                    })?;
                candidates.push(TapeInitCandidate {
                    library_serial: library.serial.clone(),
                    element_address: element,
                    location: TapeInitLocation::Slot,
                    voltag: slot.cartridge.clone(),
                });
            }
            Ok(candidates)
        }
    }
}

fn configured_report_libraries<'a>(
    report: &'a DiscoveryReport,
    config: &remanence_state::RemConfig,
    requested_library: Option<&str>,
) -> Result<Vec<&'a Library>, String> {
    let configured = config
        .libraries
        .iter()
        .map(|library| library.serial.as_str())
        .collect::<std::collections::HashSet<_>>();
    if configured.is_empty() {
        return Err("no libraries are allowlisted in config".to_string());
    }
    if let Some(serial) = requested_library {
        if !configured.contains(serial) {
            return Err(format!("library {serial:?} is not allowlisted in config"));
        }
        return report
            .library(serial)
            .map(|library| vec![library])
            .ok_or_else(|| format!("configured library {serial:?} was not discovered"));
    }
    Ok(report
        .libraries
        .iter()
        .filter(|library| configured.contains(library.serial.as_str()))
        .collect())
}

fn configured_library_policy(config: &remanence_state::RemConfig) -> StaticAllowlist {
    let mut policy = StaticAllowlist::new(config.libraries.iter().map(|lib| lib.serial.clone()));
    for library in config
        .libraries
        .iter()
        .filter(|library| library.allow_derived_drive_identity)
    {
        policy = policy.with_derived_allowed(library.serial.clone());
    }
    policy
}

fn unique_configured_library<'a>(
    report: &'a DiscoveryReport,
    config: &remanence_state::RemConfig,
    requested_library: Option<&str>,
) -> Result<&'a Library, String> {
    let libraries = configured_report_libraries(report, config, requested_library)?;
    match libraries.as_slice() {
        [library] => Ok(*library),
        [] => Err("no configured libraries were discovered".to_string()),
        _ => {
            Err("multiple configured libraries were discovered; pass --library SERIAL".to_string())
        }
    }
}

fn unique_range_library<'a>(
    report: &'a DiscoveryReport,
    config: &remanence_state::RemConfig,
    requested_library: Option<&str>,
    start: u16,
    end: u16,
) -> Result<&'a Library, String> {
    let libraries = configured_report_libraries(report, config, requested_library)?;
    let hits = libraries
        .into_iter()
        .filter(|library| {
            start >= library.layout.slot_start
                && end
                    < library
                        .layout
                        .slot_start
                        .saturating_add(library.layout.slot_count)
        })
        .collect::<Vec<_>>();
    if hits.len() == 1 {
        Ok(hits[0])
    } else if hits.is_empty() {
        Err(format!(
            "no configured discovered library contains slot range 0x{start:04x}..0x{end:04x}"
        ))
    } else {
        Err(format!(
            "slot range 0x{start:04x}..0x{end:04x} is ambiguous; pass --library"
        ))
    }
}

fn unique_or_error(
    hits: Vec<TapeInitCandidate>,
    description: String,
) -> Result<Vec<TapeInitCandidate>, String> {
    match hits.len() {
        0 => Err(format!(
            "{description} was not found in configured discovered libraries"
        )),
        1 => Ok(hits),
        _ => Err(format!(
            "{description} is ambiguous; pass --library or an element address"
        )),
    }
}

fn run_one_tape_init<S: TapeInitStateOps>(
    state: &mut S,
    ctx: &TapeInitRunContext<'_>,
    candidate: TapeInitCandidate,
    err: &mut dyn Write,
) -> Result<TapeInitRunResult, String> {
    let voltag = candidate
        .voltag
        .clone()
        .ok_or_else(|| format!("{} has no readable barcode", candidate.label()))?;
    let generation = remanence_api::lto_generation_from_voltag(voltag.as_str())
        .ok_or_else(|| format!("barcode {voltag:?} has no known LTO generation suffix"))?;
    let pool_id =
        remanence_state::derive_tape_pool_from_voltag(voltag.as_str(), &ctx.config.tape_pool_rules)
            .ok_or_else(|| format!("barcode {voltag:?} does not match any tape_pool_rule"))?
            .to_string();
    if !ctx.config.tape_pools.iter().any(|pool| pool.id == pool_id) {
        return Err(format!(
            "barcode {voltag:?} derives pool {pool_id:?}, which is not configured"
        ));
    }
    let fresh_block_size =
        resolve_tape_init_block_size(ctx.config, pool_id.as_str(), ctx.args.block_size)?;

    let library = ctx
        .report
        .library(candidate.library_serial.as_str())
        .ok_or_else(|| {
            format!(
                "library {:?} disappeared from discovery",
                candidate.library_serial
            )
        })?;
    let drive_element = select_write_compatible_drive(library, &candidate, generation)?;
    let conflicts = state.media_readiness_admission_conflicts(
        library.serial.as_str(),
        Some(drive_element),
        Some(voltag.as_str()),
        candidate.location == TapeInitLocation::Slot,
    )?;
    if !conflicts.is_empty() {
        return Err(media_readiness_admission_error("tape init", &conflicts));
    }
    run_tape_init_hardware(
        library,
        state,
        ctx.config,
        ctx.policy,
        ctx.args,
        TapeInitHardwarePlan {
            candidate,
            voltag,
            pool_id,
            generation,
            fresh_block_size,
            drive_element,
        },
        err,
    )
}

fn resolve_tape_init_block_size(
    config: &remanence_state::RemConfig,
    pool_id: &str,
    override_bytes: Option<u64>,
) -> Result<u32, String> {
    let block_size = match override_bytes {
        Some(block_size) => block_size,
        None => config
            .tape_pools
            .iter()
            .find(|pool| pool.id == pool_id)
            .map(|pool| pool.block_size_bytes)
            .unwrap_or(remanence_state::DEFAULT_TAPE_BLOCK_SIZE_BYTES),
    };
    remanence_state::validate_block_size(block_size)?;
    u32::try_from(block_size).map_err(|_| format!("block size {block_size} exceeds u32"))
}

fn select_write_compatible_drive(
    library: &Library,
    candidate: &TapeInitCandidate,
    tape_generation: remanence_api::LtoGen,
) -> Result<u16, String> {
    if candidate.location == TapeInitLocation::Drive {
        let bay = library
            .drive_bays
            .iter()
            .find(|bay| bay.element_address == candidate.element_address)
            .ok_or_else(|| {
                format!(
                    "drive element 0x{:04x} is not present in library {}",
                    candidate.element_address, library.serial
                )
            })?;
        if bay.loaded
            && bay.loaded_tape == candidate.voltag
            && drive_can_write_tape(bay, tape_generation)
        {
            return Ok(bay.element_address);
        }
        return Err(format!(
            "loaded drive 0x{:04x} is not write-compatible with {tape_generation}",
            candidate.element_address
        ));
    }

    library
        .drive_bays
        .iter()
        .filter(|bay| !bay.loaded)
        .find(|bay| drive_can_write_tape(bay, tape_generation))
        .map(|bay| bay.element_address)
        .ok_or_else(|| {
            format!(
                "no free drive in library {} is write-compatible with {tape_generation}",
                library.serial
            )
        })
}

fn drive_can_write_tape(bay: &DriveBay, tape_generation: remanence_api::LtoGen) -> bool {
    bay.installed
        .as_ref()
        .and_then(|drive| drive.product.as_deref())
        .and_then(remanence_api::lto_generation_from_drive_product)
        .is_some_and(|drive_generation| remanence_api::can_write(drive_generation, tape_generation))
}

#[cfg(target_os = "linux")]
fn run_tape_init_hardware<S: TapeInitStateOps>(
    library: &Library,
    state: &mut S,
    config: &remanence_state::RemConfig,
    policy: &StaticAllowlist,
    args: &TapeInitArgs,
    plan: TapeInitHardwarePlan,
    err: &mut dyn Write,
) -> Result<TapeInitRunResult, String> {
    let TapeInitHardwarePlan {
        candidate,
        voltag,
        pool_id,
        generation,
        fresh_block_size,
        drive_element,
    } = plan;
    let operation_id = Uuid::new_v4();
    let drive_bay = library
        .drive_bays
        .iter()
        .find(|bay| bay.element_address == drive_element);
    let signal_guard = MediaReadinessSignalGuard::install()?;
    state.record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
        operation_id,
        run_id: None,
        library_serial: library.serial.clone(),
        changer_sg: Some(library.changer_sg.display().to_string()),
        drive_element,
        drive_sg: drive_bay
            .and_then(|bay| bay.installed.as_ref())
            .and_then(|drive| drive.sg_path.as_ref())
            .map(|path| path.display().to_string()),
        drive_serial: drive_bay
            .and_then(|bay| bay.installed.as_ref())
            .map(|drive| drive.serial.clone()),
        barcode: Some(voltag.clone()),
        source_slot: if candidate.location == TapeInitLocation::Slot {
            Some(candidate.element_address)
        } else {
            drive_bay.and_then(|bay| bay.source_slot)
        },
        media_generation: Some(generation.generation_number()),
        phase: "planned".to_string(),
        state: "planned".to_string(),
        dirty_scope: Some("drive+tape".to_string()),
        deadline_at_utc: deadline_after(MEDIA_CONDITIONING_TIMEOUT),
        evidence_path: None,
    })?;
    record_media_readiness_signal_if_requested(state, operation_id, "before_open_library", || {
        signal_guard.requested_signal_name()
    })?;
    let mut handle = match open_library_handle(library, policy) {
        Ok(handle) => handle,
        Err(error) => {
            if let Some(signal) = record_media_readiness_signal_or_mechanical_failure(
                state,
                operation_id,
                "open_library",
                "transport_unknown",
                None,
                &error.to_string(),
                signal_guard.requested_signal_name(),
            )? {
                return Err(signal);
            }
            return Err(format!("opening library: {error}"));
        }
    };
    record_media_readiness_signal_if_requested(state, operation_id, "after_open_library", || {
        signal_guard.requested_signal_name()
    })?;
    if candidate.location == TapeInitLocation::Slot {
        record_media_readiness_signal_if_requested(
            state,
            operation_id,
            "before_move_medium",
            || signal_guard.requested_signal_name(),
        )?;
        state.record_media_readiness_transition(media_readiness_mechanical_transition(
            operation_id,
            "move_medium",
            "pre_ready_loading",
            Some(0xa5),
            None,
        ))?;
        if let Err(error) = handle.move_medium(candidate.element_address, drive_element, policy) {
            if let Some(signal) = record_media_readiness_signal_or_mechanical_failure(
                state,
                operation_id,
                "move_medium",
                "transport_unknown",
                Some(0xa5),
                &error.to_string(),
                signal_guard.requested_signal_name(),
            )? {
                return Err(signal);
            }
            return Err(format!(
                "move slot 0x{:04x} to drive 0x{drive_element:04x}: {error}",
                candidate.element_address
            ));
        }
        record_media_readiness_signal_if_requested(
            state,
            operation_id,
            "after_move_medium",
            || signal_guard.requested_signal_name(),
        )?;
    }
    record_media_readiness_signal_if_requested(state, operation_id, "before_open_drive", || {
        signal_guard.requested_signal_name()
    })?;
    let mut drive = match handle.open_drive(drive_element, policy) {
        Ok(drive) => drive,
        Err(error) => {
            if let Some(signal) = record_media_readiness_signal_or_mechanical_failure(
                state,
                operation_id,
                "open_drive",
                "transport_unknown",
                None,
                &error.to_string(),
                signal_guard.requested_signal_name(),
            )? {
                return Err(signal);
            }
            return Err(format!("open drive 0x{drive_element:04x}: {error}"));
        }
    };
    record_media_readiness_signal_if_requested(state, operation_id, "after_open_drive", || {
        signal_guard.requested_signal_name()
    })?;
    let family = media_family_for_init_generation(generation);
    let readiness = {
        let initial_phase = if candidate.location == TapeInitLocation::Slot {
            "pre_load_tur"
        } else {
            "already_loaded_tur"
        };
        let before_initial_phase = format!("before_{initial_phase}");
        record_media_readiness_signal_if_requested(
            state,
            operation_id,
            before_initial_phase.as_str(),
            || signal_guard.requested_signal_name(),
        )?;
        let initial = drive.probe_media_readiness(family);
        let initial_poll =
            poll_already_observed_media_readiness(initial, initial_phase, operation_id, state)?;
        let after_initial_phase = format!("after_{initial_phase}");
        record_media_readiness_signal_if_requested(
            state,
            operation_id,
            after_initial_phase.as_str(),
            || signal_guard.requested_signal_name(),
        )?;
        poll_tape_init_readiness_after_initial_probe(
            &mut drive,
            family,
            operation_id,
            state,
            drive_element,
            MediaReadinessInitialProbeInput {
                initial_poll,
                wait: true,
                timeout: MEDIA_CONDITIONING_TIMEOUT,
                poll_interval: MEDIA_CONDITIONING_STEADY_POLL,
                conditional_load_on_no_medium: candidate.location == TapeInitLocation::Slot,
            },
            || signal_guard.requested_signal_name(),
        )?
    };
    if !readiness.readiness.is_ready() {
        return Err(tape_init_readiness_error(
            voltag.as_str(),
            drive_element,
            &readiness,
        ));
    }
    record_media_readiness_signal_if_requested(
        state,
        operation_id,
        "before_read_drive_config",
        || signal_guard.requested_signal_name(),
    )?;
    let drive_config = match drive.read_config() {
        Ok(config) => config,
        Err(error) => {
            if let Some(signal) = record_media_readiness_signal_or_command_failure(
                state,
                operation_id,
                "read_drive_config",
                None,
                &error.to_string(),
                signal_guard.requested_signal_name(),
            )? {
                return Err(signal);
            }
            return Err(format!("read drive config: {error}"));
        }
    };
    if drive_config.write_protected {
        return Err("media is write-protected according to MODE SENSE".to_string());
    }

    record_media_readiness_signal_if_requested(
        state,
        operation_id,
        "before_rewind_before_bot_read",
        || signal_guard.requested_signal_name(),
    )?;
    if let Err(error) = drive.rewind() {
        if let Some(signal) = record_media_readiness_signal_or_command_failure(
            state,
            operation_id,
            "rewind_before_bot_read",
            Some(0x01),
            &error.to_string(),
            signal_guard.requested_signal_name(),
        )? {
            return Err(signal);
        }
        return Err(format!("rewind before BOT read: {error}"));
    }
    record_media_readiness_signal_if_requested(
        state,
        operation_id,
        "after_rewind_before_bot_read",
        || signal_guard.requested_signal_name(),
    )?;
    let bot_projection = {
        let mut source = DriveHandleSource(&mut drive);
        remanence_api::classify_bot_from_source(&mut source)
    };
    record_media_readiness_signal_if_requested(
        state,
        operation_id,
        "after_bot_classification",
        || signal_guard.requested_signal_name(),
    )?;
    if matches!(drive_config.worm, WormMediaState::Worm)
        && !matches!(
            bot_projection.classification,
            remanence_api::BotClassification::BlankCheckEod
        )
    {
        return Err(
            "WORM media already contains data/bootstrap; init refuses to append or rewrite it"
                .to_string(),
        );
    }

    let catalog_projection = state.project_catalog_inputs(
        voltag.as_str(),
        &bot_projection.classification,
        pool_id.as_str(),
    )?;
    let decision = remanence_api::decide_tape_init(
        &bot_projection.classification,
        catalog_projection.catalog_row.as_ref(),
        &catalog_projection.barcode_state,
        pool_id.as_str(),
        bot_projection.physical_data_past_bootstrap,
        &catalog_projection.committed_copies,
    );
    let clobber_data_confirmed = if should_confirm_clobber(args, &decision) {
        let confirmed = match confirm_clobber_data(voltag.as_str(), &decision, err) {
            Ok(confirmed) => confirmed,
            Err(error) => {
                if let Some(signal) = signal_guard.requested_signal_name() {
                    record_media_readiness_signal_transition(
                        state,
                        operation_id,
                        "clobber_confirmation",
                        signal,
                    )?;
                    return Err(media_readiness_signal_abort_message(signal));
                }
                return Err(error);
            }
        };
        record_media_readiness_signal_if_requested(
            state,
            operation_id,
            "after_clobber_confirmation",
            || signal_guard.requested_signal_name(),
        )?;
        confirmed
    } else {
        false
    };
    let (planned_uuid, block_size, parity) = planned_init_geometry(
        &bot_projection.classification,
        &decision,
        clobber_data_confirmed,
        fresh_block_size,
    );
    record_media_readiness_signal_if_requested(
        state,
        operation_id,
        "before_rewind_before_bootstrap_write",
        || signal_guard.requested_signal_name(),
    )?;
    // BOT classification reads at least the bootstrap block and may probe past
    // it for data. Rewind again so a fresh init overwrites the bootstrap at
    // block 0 rather than appending a new bootstrap after the probe reads.
    if let Err(error) = drive.rewind() {
        if let Some(signal) = record_media_readiness_signal_or_command_failure(
            state,
            operation_id,
            "rewind_before_bootstrap_write",
            Some(0x01),
            &error.to_string(),
            signal_guard.requested_signal_name(),
        )? {
            return Err(signal);
        }
        return Err(format!("rewind before bootstrap write: {error}"));
    }
    record_media_readiness_signal_if_requested(
        state,
        operation_id,
        "before_apply_init_write_gate",
        || signal_guard.requested_signal_name(),
    )?;
    let action_result = {
        let mut sink = DriveHandleSink(&mut drive);
        remanence_api::maybe_write_tape_init_bootstrap(
            &mut sink,
            &decision,
            remanence_api::TapeInitWriteOptions {
                dry_run: args.dry_run,
                force: args.force,
                clobber_data_confirmed,
            },
            planned_uuid,
            block_size,
            parity.clone(),
            env!("CARGO_PKG_VERSION"),
        )
    };
    let action = match action_result {
        Ok(action) => action,
        Err(error) => {
            if let Some(signal) = record_media_readiness_signal_or_command_failure(
                state,
                operation_id,
                "apply_init_write_gate",
                None,
                &error.to_string(),
                signal_guard.requested_signal_name(),
            )? {
                return Err(signal);
            }
            return Err(format!("apply init write gate: {error}"));
        }
    };
    if action == remanence_api::TapeInitWriteAction::WroteBootstrap {
        state.provision_initialized_tape(
            config,
            planned_uuid,
            voltag.clone(),
            block_size,
            parity.clone(),
            args.force || clobber_data_confirmed,
        )?;
    }

    Ok(TapeInitRunResult {
        success: action != remanence_api::TapeInitWriteAction::Refused || args.dry_run,
        uuid: planned_uuid,
        voltag,
        pool_id,
        generation,
        capacity_bytes: remanence_api::raw_capacity_bytes(generation),
        block_size,
        parity,
        library_serial: candidate.library_serial,
        source_element: candidate.element_address,
        drive_element,
        decision,
        action,
        notes: catalog_projection.notes,
    })
}

#[cfg(not(target_os = "linux"))]
fn run_tape_init_hardware<S: TapeInitStateOps>(
    _library: &Library,
    _state: &mut S,
    _config: &remanence_state::RemConfig,
    _policy: &StaticAllowlist,
    _args: &TapeInitArgs,
    _plan: TapeInitHardwarePlan,
    _err: &mut dyn Write,
) -> Result<TapeInitRunResult, String> {
    Err("tape init requires Linux SG_IO drive access in v0.1".to_string())
}

fn tape_init_readiness_error(
    voltag: &str,
    drive_element: u16,
    poll: &MediaReadinessPoll,
) -> String {
    let timeout = if poll.timed_out {
        format!(
            " after {}s readiness wait",
            MEDIA_CONDITIONING_TIMEOUT.as_secs()
        )
    } else {
        String::new()
    };
    let state = media_readiness_state_name(&poll.readiness, poll.timed_out);
    let exit_code = tape_init_readiness_exit_code(poll);
    format!(
        "media not ready for tape init on {voltag} in drive 0x{drive_element:04x}{timeout} media_readiness_state={state} media_readiness_exit_code={exit_code}: {}; leave the tape in the drive and run `rem tape wait-ready` or fieldtest/scripts/09-media-ready.sh before retrying init",
        describe_media_readiness(&poll.readiness)
    )
}

fn media_family_for_init_generation(generation: remanence_api::LtoGen) -> MediaFamily {
    if generation.generation_number() >= 9 {
        MediaFamily::Lto9OrLater
    } else {
        MediaFamily::Unknown
    }
}

fn describe_media_readiness(readiness: &MediaReadiness) -> String {
    match readiness {
        MediaReadiness::Ready => "ready".to_string(),
        MediaReadiness::BecomingReady {
            ascq,
            media_initializing,
        } => {
            if *media_initializing {
                format!(
                    "media initializing/calibrating (TEST UNIT READY sense 02/04/{ascq:02x}); retry after the library UI leaves Calib/initializing"
                )
            } else {
                format!(
                    "logical unit becoming ready (TEST UNIT READY sense 02/04/{ascq:02x}); retry later"
                )
            }
        }
        MediaReadiness::NoMedium { ascq } => {
            format!("drive reports no medium (TEST UNIT READY sense 02/3a/{ascq:02x})")
        }
        MediaReadiness::UnitAttention { asc, ascq } => {
            format!("unit attention during readiness probe (sense 06/{asc:02x}/{ascq:02x})")
        }
        MediaReadiness::RepeatedUnitAttention { asc, ascq } => {
            format!("repeated unit attention during readiness probe (sense 06/{asc:02x}/{ascq:02x}); fence for recovery")
        }
        MediaReadiness::TerminalNotReady { ascq, action } => {
            format!("terminal not-ready state {action} (sense 02/04/{ascq:02x})")
        }
        MediaReadiness::CheckCondition { key, asc, ascq } => {
            format!("readiness probe check condition (sense {key:02x}/{asc:02x}/{ascq:02x})")
        }
        MediaReadiness::UndecodedCheckCondition { sense } => {
            format!("readiness probe returned undecoded check condition sense={sense:02x?}")
        }
        MediaReadiness::TargetBusy { status } => {
            format!("target busy during readiness probe (status 0x{status:02x}); retry later")
        }
        MediaReadiness::ReservationConflict => {
            "reservation conflict during readiness probe; another owner holds the drive".to_string()
        }
        MediaReadiness::TaskAborted => {
            "task aborted during readiness probe; fence this operation for recovery".to_string()
        }
        MediaReadiness::UnexpectedStatus { status } => {
            format!("unexpected target status during readiness probe: 0x{status:02x}")
        }
        MediaReadiness::TransportUnknown { detail } => {
            format!("transport completion unknown during readiness probe: {detail}")
        }
        MediaReadiness::InvalidRequest { detail } => {
            format!("invalid readiness probe request: {detail}")
        }
    }
}

fn should_confirm_clobber(args: &TapeInitArgs, decision: &remanence_api::InitDecision) -> bool {
    args.clobber_data
        && matches!(
            decision,
            remanence_api::InitDecision::RefuseClobber {
                reason: remanence_api::TapeInitError::ForeignFormat(_)
                    | remanence_api::TapeInitError::UnrecognizedData
                    | remanence_api::TapeInitError::PhysicalDataPastBootstrap
                    | remanence_api::TapeInitError::CommittedCopiesPresent
                    | remanence_api::TapeInitError::CatalogIndicatesWritten
            }
        )
}

fn confirm_clobber_data(
    voltag: &str,
    decision: &remanence_api::InitDecision,
    err: &mut dyn Write,
) -> Result<bool, String> {
    let expected = format!("CLOBBER {voltag}");
    let _ = writeln!(
        err,
        "danger: {voltag} would clobber data ({})",
        init_decision_label(decision)
    );
    let _ = writeln!(err, "type {expected:?} to continue:");
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .map_err(|error| format!("read clobber confirmation: {error}"))?;
    if line.trim() == expected {
        Ok(true)
    } else {
        Err("clobber confirmation did not match; no write attempted".to_string())
    }
}

fn planned_init_geometry(
    bot: &remanence_api::BotClassification,
    decision: &remanence_api::InitDecision,
    clobber_data_confirmed: bool,
    fresh_block_size: u32,
) -> (remanence_api::TapeUuid, u32, remanence_api::ParityConfig) {
    if let remanence_api::BotClassification::OursBootstrap { uuid, geometry } = bot {
        if matches!(
            decision,
            remanence_api::InitDecision::IdempotentNoOp
                | remanence_api::InitDecision::RequireForce { .. }
        ) && !clobber_data_confirmed
        {
            return (*uuid, geometry.block_size_bytes, geometry.parity.clone());
        }
    }
    (
        *Uuid::new_v4().as_bytes(),
        fresh_block_size,
        remanence_api::ParityConfig::None,
    )
}

fn pool_projection_inputs(
    config: &remanence_state::RemConfig,
) -> Vec<remanence_state::TapePoolProjectionInput> {
    config
        .tape_pools
        .iter()
        .map(|pool| remanence_state::TapePoolProjectionInput {
            pool_id: pool.id.clone(),
            display_name: pool.display_name.clone(),
            copy_class: pool.copy_class.clone(),
            content_class: pool.content_class.clone(),
            created_at_utc: None,
        })
        .collect()
}

fn print_tape_init_result(result: &TapeInitRunResult, out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "tape init {}: {}",
        result.voltag,
        tape_init_action_label(result.action)
    );
    let _ = writeln!(out, "  uuid: {}", Uuid::from_bytes(result.uuid));
    let _ = writeln!(out, "  library: {}", result.library_serial);
    let _ = writeln!(
        out,
        "  source: 0x{:04x}; drive: 0x{:04x}",
        result.source_element, result.drive_element
    );
    let _ = writeln!(out, "  pool: {}", result.pool_id);
    let _ = writeln!(out, "  generation: {}", result.generation);
    let _ = writeln!(out, "  capacity_bytes: {}", result.capacity_bytes);
    let _ = writeln!(
        out,
        "  geometry: block_size={} parity={}",
        result.block_size,
        parity_label(&result.parity)
    );
    let _ = writeln!(out, "  decision: {}", init_decision_label(&result.decision));
    for note in &result.notes {
        let _ = writeln!(out, "  note: {note}");
    }
}

fn tape_init_action_label(action: remanence_api::TapeInitWriteAction) -> &'static str {
    match action {
        remanence_api::TapeInitWriteAction::WroteBootstrap => "wrote-bootstrap",
        remanence_api::TapeInitWriteAction::DryRunWouldWrite => "dry-run-would-write",
        remanence_api::TapeInitWriteAction::IdempotentNoOp => "idempotent-no-op",
        remanence_api::TapeInitWriteAction::Refused => "refused-no-write",
    }
}

fn parity_label(parity: &remanence_api::ParityConfig) -> String {
    match parity {
        remanence_api::ParityConfig::None => "none".to_string(),
        remanence_api::ParityConfig::Scheme(scheme) => scheme.id.as_str().to_string(),
    }
}

fn init_decision_label(decision: &remanence_api::InitDecision) -> String {
    match decision {
        remanence_api::InitDecision::FreshInit => "fresh-init".to_string(),
        remanence_api::InitDecision::IdempotentNoOp => "idempotent-no-op".to_string(),
        remanence_api::InitDecision::RequireForce { reason } => {
            format!("require-force: {}", init_error_label(reason))
        }
        remanence_api::InitDecision::RefuseClobber { reason } => {
            format!("refuse-clobber: {}", init_error_label(reason))
        }
        remanence_api::InitDecision::Anomaly { reason } => {
            format!("anomaly: {}", init_error_label(reason))
        }
        remanence_api::InitDecision::NeedsExplicitRebuild { reason } => {
            format!("needs-explicit-rebuild: {}", init_error_label(reason))
        }
    }
}

fn init_error_label(error: &remanence_api::TapeInitError) -> String {
    match error {
        remanence_api::TapeInitError::BarcodeAssignedToDifferentUuid {
            assigned_uuid,
            bot_uuid,
        } => {
            format!(
                "barcode assigned to {}; BOT uuid={}",
                Uuid::from_bytes(*assigned_uuid),
                bot_uuid
                    .map(Uuid::from_bytes)
                    .map(|uuid| uuid.to_string())
                    .unwrap_or_else(|| "none".to_string())
            )
        }
        remanence_api::TapeInitError::BarcodeRetired => "barcode retired".to_string(),
        remanence_api::TapeInitError::MediaSwapReusedBarcode {
            bot_uuid,
            catalog_uuid,
        } => format!(
            "media swap/reused barcode: BOT {} catalog {}",
            Uuid::from_bytes(*bot_uuid),
            Uuid::from_bytes(*catalog_uuid)
        ),
        remanence_api::TapeInitError::BarcodeChangedWithoutRelabel => {
            "barcode changed without relabel record".to_string()
        }
        remanence_api::TapeInitError::BotReadError => "BOT read error".to_string(),
        remanence_api::TapeInitError::ForeignFormat(name) => format!("foreign format {name}"),
        remanence_api::TapeInitError::UnrecognizedData => "unrecognized BOT data".to_string(),
        remanence_api::TapeInitError::MissingCatalogRow { bot_uuid } => {
            format!("missing catalog row for {}", Uuid::from_bytes(*bot_uuid))
        }
        remanence_api::TapeInitError::PhysicalDataPastBootstrap => {
            "physical data past bootstrap".to_string()
        }
        remanence_api::TapeInitError::CommittedCopiesPresent => {
            "committed copies present".to_string()
        }
        remanence_api::TapeInitError::TapePoolAssignmentConflict {
            committed_pool,
            derived_pool,
        } => format!(
            "pool assignment conflict: committed={} derived={derived_pool}",
            committed_pool.as_deref().unwrap_or("unknown")
        ),
        remanence_api::TapeInitError::GeometryMismatch => "geometry mismatch".to_string(),
        remanence_api::TapeInitError::CatalogIndicatesWritten => {
            "catalog indicates written tape".to_string()
        }
    }
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
    if record_size % BRU_BLOCK_SIZE != 0 {
        return Err(format!(
            "record size {record_size} must be a multiple of BRU block size {BRU_BLOCK_SIZE}"
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
    if dump_len % BRU_BLOCK_SIZE as u64 != 0 {
        return Err(format!(
            "dump length {dump_len} is not a multiple of BRU block size {BRU_BLOCK_SIZE}"
        ));
    }

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
            block_size: BlockSize::Variable,
            compression: false,
            max_block_size_bytes: current_config.max_block_size_bytes,
            write_protected: current_config.write_protected,
            worm: current_config.worm,
        })
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
        if filled % BRU_BLOCK_SIZE != 0 {
            return Err(format!(
                "final record length {filled} is not a multiple of BRU block size {BRU_BLOCK_SIZE}"
            ));
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

fn run_archive_dump_command(
    command: &ArchiveCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let format = command.format();
    match command {
        ArchiveCommand::Probe(args) => {
            let path = match dump_path(&args.source, err) {
                Ok(path) => path,
                Err(code) => return code,
            };
            let mut reader = match open_dump_reader(path, format, err) {
                Ok(reader) => reader,
                Err(code) => return code,
            };
            match probe_dump_archive(format, &mut reader) {
                Ok(probe) => {
                    print_probe(&probe, out);
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        ArchiveCommand::Scan(args) => {
            let path = match dump_path(&args.source, err) {
                Ok(path) => path,
                Err(code) => return code,
            };
            let reader = match open_dump_reader(path, format, err) {
                Ok(reader) => reader,
                Err(code) => return code,
            };
            let mut archive = match open_dump_archive(format, reader) {
                Ok(archive) => archive,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return ExitCode::from(1);
                }
            };
            match remanence_stream::scan_archive_reader(archive.as_mut()) {
                Ok(report) => {
                    print_archive_scan(&report, out);
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        ArchiveCommand::Restore(args) => {
            let path = match dump_path(&args.source, err) {
                Ok(path) => path,
                Err(code) => return code,
            };
            let reader = match open_dump_reader(path, format, err) {
                Ok(reader) => reader,
                Err(code) => return code,
            };
            let mut archive = match open_dump_archive(format, reader) {
                Ok(archive) => archive,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return ExitCode::from(1);
                }
            };
            match remanence_stream::restore_archive_reader_to_directory(
                archive.as_mut(),
                &args.dest,
                remanence_stream::FilesystemRestoreOptions {
                    overwrite: args.overwrite,
                    include_manifest: false,
                },
            ) {
                Ok(report) => {
                    print_archive_restore(&report, out);
                    print_damage_list(&report.damages, err);
                    print_archive_gap_list(&report.archive_gaps, err);
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        ArchiveCommand::Recover(args) => {
            let path = match dump_path(&args.source, err) {
                Ok(path) => path,
                Err(code) => return code,
            };
            let reader = match open_dump_reader(path, format, err) {
                Ok(reader) => reader,
                Err(code) => return code,
            };
            let mut archive = match open_dump_archive(format, reader) {
                Ok(archive) => archive,
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    return ExitCode::from(1);
                }
            };
            let source = format!("dump:{}", path.display());
            match remanence_stream::recover_archive_reader_to_directory(
                archive.as_mut(),
                &args.dest,
                remanence_stream::RecoveryOptions::new(format.driver_id(), source),
            ) {
                Ok(report) => {
                    print_archive_recovery(&report, out);
                    print_damage_list_from_recovery(&report.files, err);
                    print_recovery_archive_gap_list(&report.archive_gaps, err);
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    let _ = writeln!(err, "error: {error}");
                    ExitCode::from(1)
                }
            }
        }
        ArchiveCommand::Build(_) => {
            unreachable!("archive build dispatched before the dump handler")
        }
        ArchiveCommand::Capabilities => {
            unreachable!("archive capabilities dispatched before the dump handler")
        }
        ArchiveCommand::Reseal(_) => {
            unreachable!("archive reseal dispatched before the dump handler")
        }
        ArchiveCommand::Inspect(_) => {
            unreachable!("archive inspect dispatched before the dump handler")
        }
        ArchiveCommand::Extract(_) => {
            unreachable!("archive extract dispatched before the dump handler")
        }
        ArchiveCommand::ExtractStream(_) => {
            unreachable!("archive extract-stream dispatched before the dump handler")
        }
        ArchiveCommand::CoveringRange(_) => {
            unreachable!("archive covering-range dispatched before the dump handler")
        }
        ArchiveCommand::Write(_) => {
            unreachable!("archive write dispatched before the dump handler")
        }
        ArchiveCommand::Read(_) => {
            unreachable!("archive read dispatched before the dump handler")
        }
        ArchiveCommand::ExportObject(_) => {
            unreachable!("archive export-object dispatched before the dump handler")
        }
        ArchiveCommand::Verify(_) => {
            unreachable!("archive verify dispatched before the dump handler")
        }
        ArchiveCommand::List(_) => {
            unreachable!("archive list dispatched before the dump handler")
        }
    }
}

fn dump_path<'a>(source: &'a ArchiveSourceArgs, err: &mut dyn Write) -> Result<&'a Path, ExitCode> {
    match source.selection() {
        Ok(ArchiveSourceSelection::Dump(path)) => Ok(path),
        Ok(ArchiveSourceSelection::Tape { .. }) => {
            let _ = writeln!(err, "error: internal dispatch bug for archive tape source");
            Err(ExitCode::from(1))
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            Err(ExitCode::from(1))
        }
    }
}

fn open_dump_reader(
    path: &Path,
    format: ArchiveFormat,
    err: &mut dyn Write,
) -> Result<BufReader<File>, ExitCode> {
    File::open(path).map(BufReader::new).map_err(|error| {
        let _ = writeln!(
            err,
            "error: open {} dump {}: {error}",
            format.cli_name(),
            path.display()
        );
        ExitCode::from(1)
    })
}

fn probe_dump_archive(
    format: ArchiveFormat,
    reader: &mut BufReader<File>,
) -> Result<ProbeResult, FormatError> {
    match format {
        #[cfg(feature = "foreign-bru")]
        ArchiveFormat::Bru => BruFormat.probe_dump(reader),
        #[cfg(not(feature = "foreign-bru"))]
        ArchiveFormat::Bru => {
            let _ = reader;
            Err(format_unavailable_error(format))
        }
    }
}

fn open_dump_archive(
    format: ArchiveFormat,
    reader: BufReader<File>,
) -> Result<Box<dyn ArchiveReader>, FormatError> {
    match format {
        #[cfg(feature = "foreign-bru")]
        ArchiveFormat::Bru => Ok(Box::new(BruFormat.open_dump_reader(reader))),
        #[cfg(not(feature = "foreign-bru"))]
        ArchiveFormat::Bru => {
            drop(reader);
            Err(format_unavailable_error(format))
        }
    }
}

#[cfg(not(feature = "foreign-bru"))]
fn format_unavailable_error(format: ArchiveFormat) -> FormatError {
    FormatError::unsupported(format!(
        "format {} ({}) is not available in this build",
        format.cli_name(),
        format.driver_id()
    ))
}

/// Build a portable RAO object file from local filesystem inputs.
///
/// This is the local-file half of the RAO work order: it writes exactly one
/// RAO object byte string to `--out` using the same `BlockSink` contract as
/// tape writers, but without tape-only filemarks, bootstrap rows, or parity
/// sidecars.
fn run_archive_build(
    args: &ArchiveBuildArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match build_archive_object_file(args) {
        Ok(report) => {
            let line = serde_json::to_string(&report)
                .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"));
            let _ = writeln!(out, "{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_archive_inspect(
    args: &ArchiveInspectArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match inspect_archive_object_file(args) {
        Ok(report) => {
            let line = serde_json::to_string(&report)
                .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"));
            let _ = writeln!(out, "{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            ExitCode::from(1)
        }
    }
}

fn run_archive_extract(
    args: &ArchiveExtractArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match extract_archive_object_file(args) {
        Ok(report) => {
            let line = serde_json::to_string(&report)
                .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"));
            let _ = writeln!(out, "{line}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            ExitCode::from(1)
        }
    }
}

/// Read exactly the bounded header/key-frame/metadata prefix from an RAO stream.
fn read_rao_authenticated_prefix(input: &mut dyn Read) -> Result<Vec<u8>, String> {
    let mut header_bytes = [0u8; RAO_HEADER_LEN];
    input
        .read_exact(&mut header_bytes)
        .map_err(|error| format!("read encrypted RAO header: {error}"))?;
    let header = RaoHeader::parse(&header_bytes)
        .map_err(|error| format!("parse encrypted RAO header: {error}"))?;
    let remaining_len = usize::try_from(header.key_frame_len)
        .ok()
        .and_then(|key_len| {
            usize::try_from(header.metadata_frame_len)
                .ok()
                .and_then(|metadata_len| key_len.checked_add(metadata_len))
        })
        .ok_or_else(|| "encrypted RAO prefix length is too large for this host".to_string())?;
    let total_len = RAO_HEADER_LEN
        .checked_add(remaining_len)
        .ok_or_else(|| "encrypted RAO prefix length overflows".to_string())?;
    let mut prefix = Vec::new();
    prefix
        .try_reserve_exact(total_len)
        .map_err(|_| "cannot allocate bounded encrypted RAO prefix".to_string())?;
    prefix.extend_from_slice(&header_bytes);
    prefix.resize(total_len, 0);
    input
        .read_exact(&mut prefix[RAO_HEADER_LEN..])
        .map_err(|error| format!("read encrypted RAO authenticated prefix: {error}"))?;
    Ok(prefix)
}

fn recipient_epochs_from_prefix_json(prefix: &[u8]) -> Result<Vec<Value>, String> {
    let header_bytes: [u8; RAO_HEADER_LEN] = prefix
        .get(..RAO_HEADER_LEN)
        .ok_or_else(|| "authenticated prefix is missing the RAO header".to_string())?
        .try_into()
        .map_err(|_| "authenticated prefix is missing the RAO header".to_string())?;
    let header = RaoHeader::parse(&header_bytes)
        .map_err(|error| format!("parse authenticated prefix header: {error}"))?;
    let key_frame_end = RAO_HEADER_LEN
        .checked_add(header.key_frame_len as usize)
        .ok_or_else(|| "authenticated prefix key-frame length overflows".to_string())?;
    let frame = remanence_aead::KeyFrame::parse(
        prefix
            .get(RAO_HEADER_LEN..key_frame_end)
            .ok_or_else(|| "authenticated prefix is missing the key frame".to_string())?,
    )
    .map_err(|error| format!("parse authenticated prefix key frame: {error}"))?;
    Ok(recipient_epochs_json(&frame))
}

/// Authenticate a prefix and emit Rust-computed covering stored geometry.
fn run_archive_covering_range(
    args: &ArchiveCoveringRangeArgs,
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = (|| -> Result<Value, String> {
        let key = encrypted_stream_key(args.private_key.as_deref())?;
        let prefix = read_rao_authenticated_prefix(input)?;
        let plan = remanence_format::covering_envelope_rao_stored_range(
            &prefix,
            &key,
            args.range.start,
            args.range.len,
        )
        .map_err(|error| format!("authenticate and map encrypted RAO range: {error}"))?;
        let stored_range_end = plan
            .stored_range_start
            .and_then(|start| start.checked_add(plan.stored_range_len));
        Ok(json!({
            "command": "archive covering-range",
            "status": "ok",
            "object_id": args.object_id,
            "envelope_object_id": plan.header.object_id,
            "file_id": args.file_id,
            "plaintext_start": plan.plaintext_start,
            "plaintext_len": plan.plaintext_len,
            "first_chunk": plan.first_chunk,
            "chunk_count": plan.chunk_count,
            "stored_range_start": plan.stored_range_start,
            "stored_range_len": plan.stored_range_len,
            "stored_range_end": stored_range_end,
            "authenticated_prefix_len": prefix.len(),
        }))
    })();

    match result {
        Ok(report) => match serde_json::to_writer(&mut *out, &report)
            .and_then(|()| writeln!(out).map_err(serde_json::Error::io))
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                let _ = writeln!(err, "error: archive covering-range: {error}");
                ExitCode::from(1)
            }
        },
        Err(error) => {
            let _ = writeln!(err, "error: archive covering-range: {error}");
            ExitCode::from(1)
        }
    }
}

/// Decrypt one complete or explicitly ranged encrypted RAO stream to `out`.
///
/// `remanence_aead::open` authenticates each payload chunk before invoking the
/// writer. The optional writer below only slices that already-authenticated
/// plaintext; it never changes AEAD framing or final-chunk validation.
fn run_archive_extract_stream(
    args: &ArchiveExtractStreamArgs,
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = (|| -> Result<Value, String> {
        let key = encrypted_stream_key(args.private_key.as_deref())?;
        if let (Some(prefix_path), Some(stored_range_start), Some(range)) = (
            args.authenticated_prefix.as_deref(),
            args.stored_range_start,
            args.range,
        ) {
            let mut prefix_file = File::open(prefix_path).map_err(|error| {
                format!(
                    "open authenticated prefix {}: {error}",
                    prefix_path.display()
                )
            })?;
            let prefix = read_rao_authenticated_prefix(&mut prefix_file)?;
            let report = remanence_format::open_envelope_rao_range_from_reader(
                &prefix,
                input,
                stored_range_start,
                out,
                &key,
                range.start,
                range.len,
            )
            .map_err(|error| format!("decrypt ranged RAO stream: {error}"))?;
            out.flush()
                .map_err(|error| format!("flush plaintext stdout: {error}"))?;
            return Ok(json!({
                "command": "archive extract-stream",
                "status": "ok",
                "mode": "ranged-ciphertext",
                "object_id": report.header.object_id,
                "recipient_epochs": recipient_epochs_from_prefix_json(&prefix)?,
                "chunk_size": report.header.chunk_size,
                "plaintext_size_bytes": report.metadata.plaintext_size,
                "plaintext_sha256": bytes_to_hex(&report.metadata.plaintext_digest),
                "bytes_written": report.plaintext_len,
                "authenticated_chunks": report.chunk_count,
                "stored_range_start": report.stored_range_start,
                "stored_range_len": report.stored_range_len,
                "range": {
                    "start": range.start,
                    "len": range.len,
                },
            }));
        }
        let (report, bytes_written) = if let Some(range) = args.range {
            let requested_end = range
                .start
                .checked_add(range.len)
                .ok_or_else(|| "--range arithmetic overflow".to_string())?;
            let mut selected = PlaintextRangeWriter::new(out, range.start, requested_end);
            let report = remanence_format::open_envelope_rao_stream(input, &mut selected, &key)
                .map_err(|error| format!("decrypt RAO stream: {error}"))?;
            selected
                .flush()
                .map_err(|error| format!("flush plaintext stdout: {error}"))?;
            if requested_end > report.plaintext.size {
                return Err(format!(
                    "--range {}:{} extends past plaintext size {}",
                    range.start, range.len, report.plaintext.size
                ));
            }
            (report, selected.bytes_written())
        } else {
            let report = remanence_format::open_envelope_rao_stream(input, &mut *out, &key)
                .map_err(|error| format!("decrypt RAO stream: {error}"))?;
            out.flush()
                .map_err(|error| format!("flush plaintext stdout: {error}"))?;
            let size = report.plaintext.size;
            (report, size)
        };

        Ok(json!({
            "command": "archive extract-stream",
            "status": "ok",
            "object_id": report.header.object_id,
            "recipient_epochs": recipient_epochs_json(&report.key_frame),
            "format_version": report.header.format_version,
            "chunk_size": report.header.chunk_size,
            "stored_size_bytes": report.stored_size_bytes,
            "plaintext_size_bytes": report.plaintext.size,
            "plaintext_sha256": bytes_to_hex(&report.plaintext.digest),
            "bytes_written": bytes_written,
            "range": args.range.map(|range| json!({
                "start": range.start,
                "len": range.len,
            })),
        }))
    })();

    match result {
        Ok(report) => {
            let line = serde_json::to_string(&report)
                .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"));
            if writeln!(err, "{line}").is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            let _ = writeln!(err, "error: archive extract-stream: {error}");
            ExitCode::from(1)
        }
    }
}

/// Streaming selector for an absolute byte range in authenticated plaintext.
struct PlaintextRangeWriter<'a> {
    inner: &'a mut dyn Write,
    start: u64,
    end: u64,
    position: u64,
    bytes_written: u64,
}

impl<'a> PlaintextRangeWriter<'a> {
    fn new(inner: &'a mut dyn Write, start: u64, end: u64) -> Self {
        Self {
            inner,
            start,
            end,
            position: 0,
            bytes_written: 0,
        }
    }

    fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

impl Write for PlaintextRangeWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let buf_len = u64::try_from(buf.len())
            .map_err(|_| io::Error::new(ErrorKind::InvalidData, "plaintext chunk too large"))?;
        let chunk_end = self
            .position
            .checked_add(buf_len)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "plaintext offset overflow"))?;
        let selected_start = self.position.max(self.start);
        let selected_end = chunk_end.min(self.end);
        if selected_start < selected_end {
            let local_start = usize::try_from(selected_start - self.position).map_err(|_| {
                io::Error::new(ErrorKind::InvalidData, "plaintext range offset too large")
            })?;
            let local_end = usize::try_from(selected_end - self.position).map_err(|_| {
                io::Error::new(ErrorKind::InvalidData, "plaintext range offset too large")
            })?;
            self.inner.write_all(&buf[local_start..local_end])?;
            self.bytes_written = self
                .bytes_written
                .checked_add(selected_end - selected_start)
                .ok_or_else(|| {
                    io::Error::new(ErrorKind::InvalidData, "plaintext byte count overflow")
                })?;
        }
        self.position = chunk_end;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug, Clone)]
struct ArchiveBuildInputFile {
    source_path: PathBuf,
    entry_type: RemTarEntryType,
    archive_path: String,
    file_id: String,
    size_bytes: u64,
    file_sha256: Option<[u8; 32]>,
    link_target: Option<String>,
    xattrs: BTreeMap<String, Vec<u8>>,
    ingest_item_id: Option<String>,
}

fn build_archive_object_file(args: &ArchiveBuildArgs) -> Result<Value, String> {
    if args.map.is_some() {
        if args.scan_only {
            return Err("--map cannot be combined with --scan-only".to_string());
        }
        if args.rules.is_some() {
            return Err("--map cannot be combined with --rules".to_string());
        }
        if !args.inputs.is_empty() {
            return Err("--map cannot be combined with --inputs".to_string());
        }
        if args.source_root.is_none() {
            return Err("--map requires --source-root".to_string());
        }
    } else if args.map_sha256.is_some() {
        return Err("--map-sha256 requires --map".to_string());
    }

    let tuning = if args.map.is_some() {
        None
    } else {
        Some(archive_scan_tuning(args)?)
    };
    if args.manifest_out.is_some() && args.rules.is_none() && args.map.is_none() {
        return Err(
            "--manifest-out requires --rules so exclusions and wrapper members are recorded"
                .to_string(),
        );
    }
    if args.scan_only {
        if !args.recipients.is_empty() {
            return Err("--scan-only cannot be combined with encryption options".to_string());
        }
        if args.manifest_out.is_some() {
            return Err(
                "--manifest-out requires a real archive build, not --scan-only".to_string(),
            );
        }
        let report = archive_ingest::scan_only_report(
            &args.inputs,
            args.rules.as_deref(),
            args.no_index,
            tuning.expect("non-map archive build has scan tuning"),
        )?;
        return serde_json::to_value(report)
            .map_err(|error| format!("serialize scan report: {error}"));
    }
    let out_path = args
        .out
        .as_ref()
        .ok_or_else(|| "--out is required unless --scan-only is set".to_string())?;
    if out_path.exists() {
        return Err(format!("--out {} already exists", out_path.display()));
    }
    if let Some(manifest_out) = &args.manifest_out {
        if manifest_out.exists() {
            return Err(format!(
                "--manifest-out {} already exists",
                manifest_out.display()
            ));
        }
    }
    let object_id = args
        .object_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let caller_object_id = args
        .caller_object_id
        .clone()
        .unwrap_or_else(|| object_id.clone());
    let manifest_file_id = args
        .manifest_file_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    if !args.recipients.is_empty() {
        object_id_field(&object_id)
            .map_err(|error| format!("--object-id is invalid for encrypted RAO: {error}"))?;
    }

    let map_plan = match (&args.map, &args.source_root) {
        (Some(map), Some(source_root)) => Some(archive_map::load_source_map(
            map,
            source_root,
            args.map_sha256.as_deref(),
        )?),
        _ => None,
    };
    let materialized = if args.rules.is_some() {
        Some(archive_ingest::materialize_inputs(
            &args.inputs,
            args.rules.as_deref(),
            args.no_index,
            tuning.expect("rules archive build has scan tuning"),
        )?)
    } else {
        None
    };
    let inputs = match (&map_plan, &materialized) {
        (Some(plan), _) => plan.inputs.clone(),
        (None, Some(plan)) => plan.inputs.clone(),
        (None, None) => collect_archive_build_inputs(&args.inputs)?,
    };
    if inputs.is_empty() {
        return Err(if args.map.is_some() {
            "--map did not contain any member rows".to_string()
        } else {
            "--inputs did not contain any archivable entries".to_string()
        });
    }

    let timestamp = match &args.timestamp {
        Some(value) => {
            OffsetDateTime::parse(value, &Rfc3339)
                .map_err(|error| format!("--timestamp must be RFC3339: {error}"))?;
            value.clone()
        }
        None => OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|error| format!("format current timestamp: {error}"))?,
    };

    let mut options = RemTarObjectOptions::new(
        object_id.clone(),
        caller_object_id.clone(),
        timestamp,
        manifest_file_id,
    );
    options.chunk_size = args.chunk_size;

    let temp_path = temporary_archive_output_path(out_path);
    if temp_path.exists() {
        return Err(format!(
            "temporary output path {} already exists",
            temp_path.display()
        ));
    }

    let build_result = (|| {
        let mut sink = FileBlockSink::create(&temp_path, args.chunk_size)
            .map_err(|error| format!("create {}: {error}", temp_path.display()))?;
        if !args.recipients.is_empty() {
            let recipients = read_recipient_public_key_files(&args.recipients)?;
            let mut readers = open_archive_build_readers(&inputs)?;
            let mut streams = archive_build_streams(&inputs, &mut readers);
            let report = remanence_format::write_encrypted_rao_object_from_readers(
                &mut sink,
                &options,
                &mut streams,
                &recipients,
            )
            .map_err(|error| format!("write encrypted RAO: {error}"))?;
            sink.sync_all()
                .map_err(|error| format!("sync {}: {error}", temp_path.display()))?;
            Ok(ArchiveBuildResult {
                layout: report.plaintext_layout,
                representation: "encrypted",
                encryption: "RAO1",
                format_version: Some(2),
                recipient_epochs: Some(recipient_epochs_json(&report.envelope.key_frame)),
                stored_digest: report.envelope.stored_digest,
                plaintext_digest: report.envelope.plaintext.digest,
                stored_size_bytes: report.envelope.stored_size_bytes,
                stored_size_blocks: report.envelope.stored_size_blocks,
            })
        } else {
            let mut readers = open_archive_build_readers(&inputs)?;
            let mut streams = archive_build_streams(&inputs, &mut readers);
            let layout = write_rem_tar_object_from_readers(&mut sink, &options, &mut streams)
                .map_err(|error| format!("write plaintext RAO: {error}"))?;
            sink.sync_all()
                .map_err(|error| format!("sync {}: {error}", temp_path.display()))?;
            let stored_digest = sha256_file(&temp_path)?;
            Ok(ArchiveBuildResult {
                stored_size_bytes: layout.total_size_bytes,
                stored_size_blocks: layout.projected_size_blocks,
                layout,
                representation: "plaintext",
                encryption: "none",
                format_version: None,
                recipient_epochs: None,
                stored_digest,
                plaintext_digest: stored_digest,
            })
        }
    })();

    let build = match build_result {
        Ok(build) => build,
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
    };
    std::fs::rename(&temp_path, out_path).map_err(|error| {
        let _ = std::fs::remove_file(&temp_path);
        format!(
            "rename {} to {}: {error}",
            temp_path.display(),
            out_path.display()
        )
    })?;
    sync_parent_directory(out_path)?;

    if let (Some(manifest_out), Some(plan)) = (&args.manifest_out, &materialized) {
        archive_ingest::write_customer_manifest(manifest_out, &plan.manifest)?;
    } else if let (Some(manifest_out), Some(plan)) = (&args.manifest_out, &map_plan) {
        archive_ingest::write_customer_manifest(manifest_out, &plan.manifest)?;
    }

    Ok(archive_build_report_json(
        args,
        &inputs,
        &build,
        materialized.as_ref(),
        map_plan.as_ref().map(|plan| plan.map_sha256),
    ))
}

fn archive_scan_tuning(args: &ArchiveBuildArgs) -> Result<archive_ingest::ScanTuning, String> {
    if !(0.0..=1.0).contains(&args.blob_suggest_ratio) || args.blob_suggest_ratio == 0.0 {
        return Err("--blob-suggest-ratio must be > 0 and <= 1".to_string());
    }
    Ok(archive_ingest::ScanTuning {
        blob_ratio: args.blob_suggest_ratio,
        blob_count: args.blob_suggest_count,
        sanity_ceiling: args.sanity_ceiling_count,
    })
}

fn sync_parent_directory(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let dir = File::open(parent)
        .map_err(|error| format!("open output directory {}: {error}", parent.display()))?;
    dir.sync_all()
        .map_err(|error| format!("sync output directory {}: {error}", parent.display()))
}

fn inspect_archive_object_file(args: &ArchiveInspectArgs) -> Result<Value, String> {
    if archive_object_is_encrypted(&args.object)? {
        inspect_encrypted_archive_object_file(&args.object)
    } else {
        inspect_plaintext_archive_object_file(&args.object, args.chunk_size)
    }
}

fn extract_archive_object_file(args: &ArchiveExtractArgs) -> Result<Value, String> {
    if archive_object_is_encrypted(&args.object)? {
        extract_encrypted_archive_object_file(args)
    } else {
        if args.private_key.is_some() {
            return Err("--private-key is only valid for encrypted RAO objects".to_string());
        }
        extract_plaintext_archive_object_file(args)
    }
}

fn inspect_plaintext_archive_object_file(path: &Path, chunk_size: usize) -> Result<Value, String> {
    let (object, stored_size_bytes, stored_size_blocks, stored_digest) =
        read_plaintext_archive_object_file(path, chunk_size)?;
    Ok(plaintext_archive_inspect_json(
        path,
        &object,
        chunk_size,
        stored_size_bytes,
        stored_size_blocks,
        stored_digest,
    ))
}

fn inspect_encrypted_archive_object_file(path: &Path) -> Result<Value, String> {
    let encrypted = read_archive_object_bytes(path)?;
    let inspected =
        inspect_bytes(&encrypted).map_err(|error| format!("inspect encrypted RAO: {error}"))?;
    Ok(encrypted_archive_keyless_json(path, &inspected))
}

fn extract_plaintext_archive_object_file(args: &ArchiveExtractArgs) -> Result<Value, String> {
    let (range_path, range) = archive_extract_range_request(args)?;
    let path = &args.object;
    let chunk_size = args.chunk_size;
    let mut source = FileBlockSource::open(path, chunk_size)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let block_count = source.block_count();
    let stored_size_bytes = source.len_bytes();
    let stored_digest = sha256_file(path)?;
    if args.blob_member.is_some() {
        if range.is_some() || range_path.is_some() {
            return Err("--blob-member cannot be combined with --path/--range".to_string());
        }
        return extract_plaintext_blob_member_file(
            args,
            BlobMemberExtractContext {
                representation: "plaintext",
                encryption: "none",
                recipient_epochs: None,
                chunk_size,
                stored_size_bytes,
                stored_size_blocks: block_count,
                stored_digest,
            },
        );
    }
    if let Some(range) = range {
        let object = read_rem_tar_object(&mut source, chunk_size, block_count)
            .map_err(|error| format!("read plaintext RAO: {error}"))?;
        let member_path = range_path.expect("range request has member path");
        return extract_plaintext_archive_range_file(
            args,
            &object,
            stored_size_bytes,
            block_count,
            stored_digest,
            member_path,
            range,
        );
    }
    let report = remanence_stream::restore_object_to_directory(
        &mut source,
        chunk_size,
        block_count,
        &args.dest,
        remanence_stream::FilesystemRestoreOptions {
            overwrite: args.overwrite,
            include_manifest: false,
        },
    )
    .map_err(|error| format!("extract plaintext RAO: {error}"))?;
    let context = ArchiveExtractReportContext {
        object: path,
        dest: &args.dest,
        representation: "plaintext",
        encryption: "none",
        recipient_epochs: None,
        chunk_size,
        stored_size_bytes,
        stored_size_blocks: block_count,
        stored_digest,
    };
    let mut value = archive_extract_report_json(&context, &report);
    attach_unwrap_report(&mut value, &args.dest, args.overwrite, args.no_unwrap)?;
    Ok(value)
}

fn encrypted_stream_key(
    private_key: Option<&Path>,
) -> Result<remanence_aead::RecipientPrivateKey, String> {
    private_key
        .ok_or_else(|| "encrypted RAO operation requires --private-key".to_string())
        .and_then(read_private_key_file)
}

fn encrypted_archive_key(
    args: &ArchiveExtractArgs,
) -> Result<remanence_aead::RecipientPrivateKey, String> {
    encrypted_stream_key(args.private_key.as_deref())
}

fn open_encrypted_archive_bytes(
    encrypted: &[u8],
    key: &remanence_aead::RecipientPrivateKey,
) -> Result<(Vec<u8>, remanence_aead::OpenReport), String> {
    let mut plaintext = Vec::new();
    let envelope = remanence_format::open_envelope_rao_stream(encrypted, &mut plaintext, key)
        .map_err(|error| format!("open encrypted RAO: {error}"))?;
    Ok((plaintext, envelope))
}

fn read_encrypted_archive_file_range(
    encrypted: &[u8],
    key: &remanence_aead::RecipientPrivateKey,
    first_chunk_lba: Option<BodyLba>,
    file_size_bytes: u64,
    range_start: u64,
    range_len: u64,
) -> Result<remanence_format::EncryptedRaoFileRange, FormatError> {
    remanence_format::read_encrypted_rao_file_range_to_vec(
        encrypted,
        key,
        first_chunk_lba,
        file_size_bytes,
        range_start,
        range_len,
    )
}

fn extract_encrypted_archive_object_file(args: &ArchiveExtractArgs) -> Result<Value, String> {
    let (range_path, range) = archive_extract_range_request(args)?;
    if args.blob_member.is_some() {
        if range.is_some() || range_path.is_some() {
            return Err("--blob-member cannot be combined with --path/--range".to_string());
        }
        return extract_encrypted_blob_member_file(args);
    }
    if let Some(range) = range {
        return extract_encrypted_archive_range_file(
            args,
            range_path.expect("range request has member path"),
            range,
        );
    }
    let encrypted = read_archive_object_bytes(&args.object)?;
    let inspected =
        inspect_bytes(&encrypted).map_err(|error| format!("inspect encrypted RAO: {error}"))?;
    let key = encrypted_archive_key(args)?;
    let (plaintext, envelope) = open_encrypted_archive_bytes(&encrypted, &key)?;
    if envelope.header != inspected.header {
        return Err("encrypted header changed between inspect and open".to_string());
    }
    let chunk_size = usize::try_from(inspected.header.chunk_size)
        .map_err(|_| "encrypted header chunk_size is too large for this host".to_string())?;
    let blocks = plaintext_blocks_from_bytes(&plaintext, chunk_size)?;
    let block_count =
        u64::try_from(blocks.len()).map_err(|_| "plaintext block count overflow".to_string())?;
    let mut parse_source = VecBlockSource::new(blocks.clone());
    let inner = read_rem_tar_object(&mut parse_source, chunk_size, block_count)
        .map_err(|error| format!("parse decrypted RAO: {error}"))?;
    let inner_object_id = required_global_pax(&inner, "REMANENCE.object_id")?;
    if inner_object_id != envelope.header.object_id {
        return Err("decrypted inner object_id does not match encrypted header".to_string());
    }
    let mut restore_source = VecBlockSource::new(blocks);
    let report = remanence_stream::restore_object_to_directory(
        &mut restore_source,
        chunk_size,
        block_count,
        &args.dest,
        remanence_stream::FilesystemRestoreOptions {
            overwrite: args.overwrite,
            include_manifest: false,
        },
    )
    .map_err(|error| format!("extract encrypted RAO: {error}"))?;
    let context = ArchiveExtractReportContext {
        object: &args.object,
        dest: &args.dest,
        representation: "encrypted",
        encryption: "RAO1",
        recipient_epochs: Some(recipient_epochs_json(&inspected.key_frame)),
        chunk_size,
        stored_size_bytes: inspected.stored_size_bytes,
        stored_size_blocks: inspected.stored_size_bytes / u64::from(inspected.header.chunk_size),
        stored_digest: inspected.stored_digest,
    };
    let mut value = archive_extract_report_json(&context, &report);
    attach_unwrap_report(&mut value, &args.dest, args.overwrite, args.no_unwrap)?;
    Ok(value)
}

fn attach_unwrap_report(
    value: &mut Value,
    dest: &Path,
    overwrite: bool,
    no_unwrap: bool,
) -> Result<(), String> {
    if no_unwrap {
        value["unwrap"] = json!({ "enabled": false });
        return Ok(());
    }
    let report = archive_ingest::unwrap_remwraps(dest, overwrite)?;
    value["unwrap"] = serde_json::to_value(report)
        .map_err(|error| format!("serialize unwrap report: {error}"))?;
    value["unwrap"]["enabled"] = json!(true);
    Ok(())
}

fn extract_encrypted_blob_member_file(args: &ArchiveExtractArgs) -> Result<Value, String> {
    let encrypted = read_archive_object_bytes(&args.object)?;
    let inspected =
        inspect_bytes(&encrypted).map_err(|error| format!("inspect encrypted RAO: {error}"))?;
    let key = encrypted_archive_key(args)?;
    let (plaintext, envelope) = open_encrypted_archive_bytes(&encrypted, &key)?;
    if envelope.header != inspected.header {
        return Err("encrypted header changed between inspect and open".to_string());
    }
    let chunk_size = usize::try_from(inspected.header.chunk_size)
        .map_err(|_| "encrypted header chunk_size is too large for this host".to_string())?;
    let scan = scan_rao_entry_locators_from_bytes(&plaintext, chunk_size)?;
    let inner_object_id = scan
        .global_pax
        .get("REMANENCE.object_id")
        .ok_or_else(|| "decrypted RAO is missing REMANENCE.object_id".to_string())?;
    if inner_object_id != &envelope.header.object_id {
        return Err("decrypted inner object_id does not match encrypted header".to_string());
    }
    extract_encrypted_blob_member_range_file(
        args,
        &encrypted,
        &key,
        &scan,
        BlobMemberExtractContext {
            representation: "encrypted",
            encryption: "RAO1",
            recipient_epochs: Some(recipient_epochs_json(&inspected.key_frame)),
            chunk_size,
            stored_size_bytes: inspected.stored_size_bytes,
            stored_size_blocks: inspected.stored_size_bytes
                / u64::from(inspected.header.chunk_size),
            stored_digest: inspected.stored_digest,
        },
    )
}

struct BlobMemberExtractContext {
    representation: &'static str,
    encryption: &'static str,
    recipient_epochs: Option<Vec<Value>>,
    chunk_size: usize,
    stored_size_bytes: u64,
    stored_size_blocks: u64,
    stored_digest: [u8; 32],
}

struct BlobMemberExtractReportContext<'a> {
    base: BlobMemberExtractContext,
    object: &'a Path,
    dest: &'a Path,
    output: &'a Path,
    blob_entry: &'a str,
    idx_entry: &'a str,
    blob_member: &'a str,
    blob_size_bytes: u64,
    idx_size_bytes: u64,
    blob_first_chunk_lba: Option<u64>,
    idx_first_chunk_lba: Option<u64>,
    blob_range_start: u64,
    blob_range_len: u64,
    idx_authenticated_chunks: Option<u64>,
    blob_authenticated_chunks: Option<u64>,
    idx_stored_range_start: Option<u64>,
    idx_stored_range_len: Option<u64>,
    blob_stored_range_start: Option<u64>,
    blob_stored_range_len: Option<u64>,
    bytes_written: u64,
}

fn blob_member_extract_report_json(context: &BlobMemberExtractReportContext<'_>) -> Value {
    json!({
        "mode": "blob-member",
        "range_method": "rao-entry-range",
        "object": context.object,
        "dest": context.dest,
        "output": context.output,
        "blob_entry": context.blob_entry,
        "idx_entry": context.idx_entry,
        "blob_member": context.blob_member,
        "representation": context.base.representation,
        "encryption": context.base.encryption,
        "recipient_epochs": context.base.recipient_epochs,
        "chunk_size": context.base.chunk_size,
        "stored_size_bytes": context.base.stored_size_bytes,
        "stored_size_blocks": context.base.stored_size_blocks,
        "stored_digest": bytes_to_hex(&context.base.stored_digest),
        "blob_size_bytes": context.blob_size_bytes,
        "idx_size_bytes": context.idx_size_bytes,
        "blob_first_chunk_lba": context.blob_first_chunk_lba,
        "idx_first_chunk_lba": context.idx_first_chunk_lba,
        "blob_range_start": context.blob_range_start,
        "blob_range_len": context.blob_range_len,
        "idx_authenticated_chunks": context.idx_authenticated_chunks,
        "blob_authenticated_chunks": context.blob_authenticated_chunks,
        "idx_stored_range_start": context.idx_stored_range_start,
        "idx_stored_range_len": context.idx_stored_range_len,
        "blob_stored_range_start": context.blob_stored_range_start,
        "blob_stored_range_len": context.blob_stored_range_len,
        "bytes_written": context.bytes_written,
    })
}

fn extract_plaintext_blob_member_file(
    args: &ArchiveExtractArgs,
    context: BlobMemberExtractContext,
) -> Result<Value, String> {
    let blob_entry = args
        .blob_entry
        .as_deref()
        .ok_or_else(|| "--blob-member requires --blob-entry".to_string())?;
    let member_path = args
        .blob_member
        .as_deref()
        .ok_or_else(|| "--blob-entry requires --blob-member".to_string())?;
    let scan = scan_plaintext_rao_entry_locators(&args.object, context.chunk_size)?;
    let idx_path = archive_ingest::remwrap_index_path(blob_entry)?;
    let idx_entry = require_regular_locator(&scan, &idx_path)?;
    let blob = require_regular_locator(&scan, blob_entry)?;
    let idx_bytes =
        read_plaintext_rao_entry_range(&args.object, idx_entry, 0, idx_entry.size_bytes)?;
    verify_locator_sha256(idx_entry, &idx_bytes, &idx_path)?;
    let member =
        archive_ingest::resolve_blob_member_from_index(&idx_bytes, &idx_path, member_path)?;
    let bytes = read_plaintext_rao_entry_range(&args.object, blob, member.offset, member.length)?;
    archive_ingest::verify_blob_member_bytes(member_path, member.sha256.as_deref(), &bytes)?;
    let output = write_blob_member_output(&args.dest, member_path, &bytes, args.overwrite)?;
    Ok(blob_member_extract_report_json(
        &BlobMemberExtractReportContext {
            base: context,
            object: &args.object,
            dest: &args.dest,
            output: &output,
            blob_entry,
            idx_entry: &idx_path,
            blob_member: member_path,
            blob_size_bytes: blob.size_bytes,
            idx_size_bytes: idx_entry.size_bytes,
            blob_first_chunk_lba: blob.first_chunk_lba.map(|lba| lba.0),
            idx_first_chunk_lba: idx_entry.first_chunk_lba.map(|lba| lba.0),
            blob_range_start: member.offset,
            blob_range_len: member.length,
            idx_authenticated_chunks: None,
            blob_authenticated_chunks: None,
            idx_stored_range_start: Some(idx_entry.data_offset),
            idx_stored_range_len: Some(idx_entry.size_bytes),
            blob_stored_range_start: Some(
                blob.data_offset
                    .checked_add(member.offset)
                    .ok_or_else(|| "blob member plaintext offset overflows".to_string())?,
            ),
            blob_stored_range_len: Some(member.length),
            bytes_written: bytes.len() as u64,
        },
    ))
}

fn extract_encrypted_blob_member_range_file(
    args: &ArchiveExtractArgs,
    encrypted: &[u8],
    key: &remanence_aead::RecipientPrivateKey,
    scan: &RaoLocatorScan,
    context: BlobMemberExtractContext,
) -> Result<Value, String> {
    let blob_entry = args
        .blob_entry
        .as_deref()
        .ok_or_else(|| "--blob-member requires --blob-entry".to_string())?;
    let member_path = args
        .blob_member
        .as_deref()
        .ok_or_else(|| "--blob-entry requires --blob-member".to_string())?;
    let idx_path = archive_ingest::remwrap_index_path(blob_entry)?;
    let idx_entry = require_regular_locator(scan, &idx_path)?;
    let blob = require_regular_locator(scan, blob_entry)?;

    let idx_range = read_encrypted_archive_file_range(
        encrypted,
        key,
        idx_entry.first_chunk_lba,
        idx_entry.size_bytes,
        0,
        idx_entry.size_bytes,
    )
    .map_err(|error| format!("extract encrypted RAO blob index range: {error}"))?;
    verify_locator_sha256(idx_entry, &idx_range.bytes, &idx_path)?;
    let member =
        archive_ingest::resolve_blob_member_from_index(&idx_range.bytes, &idx_path, member_path)?;
    let blob_range = read_encrypted_archive_file_range(
        encrypted,
        key,
        blob.first_chunk_lba,
        blob.size_bytes,
        member.offset,
        member.length,
    )
    .map_err(|error| format!("extract encrypted RAO blob member range: {error}"))?;
    archive_ingest::verify_blob_member_bytes(
        member_path,
        member.sha256.as_deref(),
        &blob_range.bytes,
    )?;
    let output =
        write_blob_member_output(&args.dest, member_path, &blob_range.bytes, args.overwrite)?;
    Ok(blob_member_extract_report_json(
        &BlobMemberExtractReportContext {
            base: context,
            object: &args.object,
            dest: &args.dest,
            output: &output,
            blob_entry,
            idx_entry: &idx_path,
            blob_member: member_path,
            blob_size_bytes: blob.size_bytes,
            idx_size_bytes: idx_entry.size_bytes,
            blob_first_chunk_lba: blob.first_chunk_lba.map(|lba| lba.0),
            idx_first_chunk_lba: idx_entry.first_chunk_lba.map(|lba| lba.0),
            blob_range_start: member.offset,
            blob_range_len: member.length,
            idx_authenticated_chunks: Some(idx_range.envelope.chunk_count),
            blob_authenticated_chunks: Some(blob_range.envelope.chunk_count),
            idx_stored_range_start: idx_range.envelope.stored_range_start,
            idx_stored_range_len: Some(idx_range.envelope.stored_range_len),
            blob_stored_range_start: blob_range.envelope.stored_range_start,
            blob_stored_range_len: Some(blob_range.envelope.stored_range_len),
            bytes_written: blob_range.bytes.len() as u64,
        },
    ))
}

#[derive(Debug, Clone)]
struct RaoLocatorScan {
    global_pax: BTreeMap<String, String>,
    entries: Vec<RaoEntryLocator>,
}

#[derive(Debug, Clone)]
struct RaoEntryLocator {
    path: String,
    entry_type: RemTarEntryType,
    size_bytes: u64,
    link_target: Option<String>,
    first_chunk_lba: Option<BodyLba>,
    data_offset: u64,
    file_sha256: Option<String>,
}

fn scan_plaintext_rao_entry_locators(
    object: &Path,
    chunk_size: usize,
) -> Result<RaoLocatorScan, String> {
    let mut file =
        File::open(object).map_err(|error| format!("open {}: {error}", object.display()))?;
    scan_rao_entry_locators(&mut file, chunk_size)
}

fn scan_rao_entry_locators_from_bytes(
    bytes: &[u8],
    chunk_size: usize,
) -> Result<RaoLocatorScan, String> {
    let mut cursor = Cursor::new(bytes);
    scan_rao_entry_locators(&mut cursor, chunk_size)
}

fn scan_rao_entry_locators<R: Read + Seek>(
    reader: &mut R,
    chunk_size: usize,
) -> Result<RaoLocatorScan, String> {
    if chunk_size == 0 {
        return Err("RAO chunk size must be nonzero".to_string());
    }
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("seek RAO object start: {error}"))?;
    let mut offset = 0u64;
    let mut global_pax = BTreeMap::new();
    let mut pending_pax = BTreeMap::new();
    let mut entries = Vec::new();
    loop {
        let mut header = [0u8; 512];
        reader
            .read_exact(&mut header)
            .map_err(|error| format!("read RAO tar header at byte {offset}: {error}"))?;
        offset = offset
            .checked_add(512)
            .ok_or_else(|| "RAO tar offset overflow".to_string())?;
        if header.iter().all(|byte| *byte == 0) {
            break;
        }
        let header_size = parse_tar_header_size(&header)?;
        match header[156] {
            b'g' | b'x' => {
                let data = read_tar_payload(reader, header_size)?;
                let records = parse_local_pax_records(&data)?;
                if header[156] == b'g' {
                    global_pax.extend(records);
                } else {
                    pending_pax = records;
                }
                let padding = tar_padding_len_local(header_size)?;
                seek_forward(reader, padding)?;
                offset = offset
                    .checked_add(round_up_512_local(header_size)?)
                    .ok_or_else(|| "RAO tar offset overflow".to_string())?;
            }
            b'0' | 0 | b'1' | b'2' | b'5' => {
                let entry_type = match header[156] {
                    b'0' | 0 => RemTarEntryType::Regular,
                    b'1' => RemTarEntryType::Hardlink,
                    b'2' => RemTarEntryType::Symlink,
                    b'5' => RemTarEntryType::Directory,
                    other => {
                        return Err(format!("unsupported RAO tar typeflag {other}"));
                    }
                };
                let path = pending_pax
                    .get("path")
                    .cloned()
                    .unwrap_or_else(|| local_tar_header_path(&header));
                let size = match pending_pax.get("size") {
                    Some(size) => size
                        .parse::<u64>()
                        .map_err(|error| format!("parse pax size for {path:?}: {error}"))?,
                    None => header_size,
                };
                if entry_type != RemTarEntryType::Regular && size != 0 {
                    return Err(format!("non-regular RAO entry {path:?} has size {size}"));
                }
                if size > 0 && offset % chunk_size as u64 != 0 {
                    return Err(format!(
                        "RAO entry {path:?} payload starts at unaligned offset {offset}"
                    ));
                }
                let link_target = if matches!(
                    entry_type,
                    RemTarEntryType::Hardlink | RemTarEntryType::Symlink
                ) {
                    let target = pending_pax
                        .get("linkpath")
                        .cloned()
                        .unwrap_or_else(|| local_tar_header_linkname(&header));
                    if target.is_empty() {
                        return Err(format!("RAO link entry {path:?} is missing target"));
                    }
                    Some(target)
                } else {
                    None
                };
                if entry_type == RemTarEntryType::Hardlink {
                    let target = link_target.as_deref().expect("hardlink target was set");
                    let target_is_primary = entries.iter().any(|entry: &RaoEntryLocator| {
                        entry.entry_type == RemTarEntryType::Regular && entry.path == target
                    });
                    if !target_is_primary {
                        return Err(format!(
                            "RAO hardlink {path:?} target {target:?} is not a preceding regular entry"
                        ));
                    }
                }
                let file_sha256 = pending_pax.get("REMANENCE.file_sha256").cloned();
                entries.push(RaoEntryLocator {
                    path,
                    entry_type,
                    size_bytes: size,
                    link_target,
                    first_chunk_lba: (size > 0).then_some(BodyLba(offset / chunk_size as u64)),
                    data_offset: offset,
                    file_sha256,
                });
                let skip = round_up_512_local(size)?;
                seek_forward(reader, skip)?;
                offset = offset
                    .checked_add(skip)
                    .ok_or_else(|| "RAO tar offset overflow".to_string())?;
                pending_pax.clear();
            }
            other => {
                return Err(format!("unsupported RAO tar typeflag {other}"));
            }
        }
    }
    Ok(RaoLocatorScan {
        global_pax,
        entries,
    })
}

fn require_regular_locator<'a>(
    scan: &'a RaoLocatorScan,
    path: &str,
) -> Result<&'a RaoEntryLocator, String> {
    let entry = scan
        .entries
        .iter()
        .find(|entry| entry.path == path)
        .ok_or_else(|| format!("RAO entry {path:?} not found"))?;
    match entry.entry_type {
        RemTarEntryType::Regular => Ok(entry),
        RemTarEntryType::Hardlink => {
            let target = entry
                .link_target
                .as_deref()
                .ok_or_else(|| format!("RAO hardlink {path:?} is missing link_target"))?;
            let primary = scan
                .entries
                .iter()
                .find(|candidate| {
                    candidate.path == target && candidate.entry_type == RemTarEntryType::Regular
                })
                .ok_or_else(|| {
                    format!("RAO hardlink {path:?} target {target:?} is not a regular primary")
                })?;
            Ok(primary)
        }
        _ => Err(format!(
            "RAO entry {path:?} is {}, not regular",
            archive_entry_type_name(entry.entry_type)
        )),
    }
}

fn read_plaintext_rao_entry_range(
    object: &Path,
    entry: &RaoEntryLocator,
    range_start: u64,
    range_len: u64,
) -> Result<Vec<u8>, String> {
    validate_blob_entry_range(entry, range_start, range_len)?;
    let byte_offset = entry
        .data_offset
        .checked_add(range_start)
        .ok_or_else(|| format!("RAO range for {:?} overflows", entry.path))?;
    let len = usize::try_from(range_len)
        .map_err(|_| format!("RAO range for {:?} is too large", entry.path))?;
    let mut file =
        File::open(object).map_err(|error| format!("open {}: {error}", object.display()))?;
    file.seek(SeekFrom::Start(byte_offset))
        .map_err(|error| format!("seek {} to byte {byte_offset}: {error}", object.display()))?;
    let mut bytes = vec![0u8; len];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("read RAO range for {:?}: {error}", entry.path))?;
    Ok(bytes)
}

fn validate_blob_entry_range(
    entry: &RaoEntryLocator,
    range_start: u64,
    range_len: u64,
) -> Result<(), String> {
    let range_end = range_start
        .checked_add(range_len)
        .ok_or_else(|| format!("RAO range for {:?} overflows", entry.path))?;
    if range_len == 0 {
        if range_start > entry.size_bytes {
            return Err(format!("empty RAO range starts past {:?}", entry.path));
        }
    } else if range_end > entry.size_bytes {
        return Err(format!("RAO range extends past {:?}", entry.path));
    }
    Ok(())
}

fn verify_locator_sha256(entry: &RaoEntryLocator, bytes: &[u8], label: &str) -> Result<(), String> {
    if let Some(expected) = &entry.file_sha256 {
        let actual = bytes_to_hex(&sha256_bytes(bytes));
        if &actual != expected {
            return Err(format!(
                "RAO entry {label:?} digest mismatch: expected {expected}, got {actual}"
            ));
        }
    }
    Ok(())
}

fn parse_tar_header_size(header: &[u8; 512]) -> Result<u64, String> {
    let field = &header[124..136];
    if field[0] & 0x80 != 0 {
        let mut value = 0u64;
        for byte in &field[1..] {
            value = value
                .checked_mul(256)
                .and_then(|value| value.checked_add(u64::from(*byte)))
                .ok_or_else(|| "tar binary size overflows u64".to_string())?;
        }
        return Ok(value);
    }
    let text = field
        .iter()
        .copied()
        .take_while(|byte| *byte != 0 && *byte != b' ')
        .collect::<Vec<_>>();
    let text = String::from_utf8_lossy(&text);
    u64::from_str_radix(text.trim(), 8).map_err(|error| format!("parse tar size {text:?}: {error}"))
}

fn read_tar_payload<R: Read>(reader: &mut R, size: u64) -> Result<Vec<u8>, String> {
    let len = usize::try_from(size).map_err(|_| "tar payload too large for this host")?;
    let mut data = vec![0u8; len];
    reader
        .read_exact(&mut data)
        .map_err(|error| format!("read tar payload: {error}"))?;
    Ok(data)
}

fn parse_local_pax_records(data: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let mut records = BTreeMap::new();
    let mut cursor = 0usize;
    while cursor < data.len() {
        let space = data[cursor..]
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| "pax record is missing length separator".to_string())?;
        let len_text = String::from_utf8_lossy(&data[cursor..cursor + space]);
        let len = len_text
            .parse::<usize>()
            .map_err(|error| format!("parse pax record length {len_text:?}: {error}"))?;
        if len == 0 || cursor + len > data.len() || cursor + space + 1 >= cursor + len {
            return Err("pax record length is invalid".to_string());
        }
        let record = &data[cursor + space + 1..cursor + len - 1];
        let eq = record
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or_else(|| "pax record is missing '='".to_string())?;
        let key = String::from_utf8(record[..eq].to_vec())
            .map_err(|error| format!("pax key is not UTF-8: {error}"))?;
        let value = String::from_utf8(record[eq + 1..].to_vec())
            .map_err(|error| format!("pax value for {key:?} is not UTF-8: {error}"))?;
        records.insert(key, value);
        cursor += len;
    }
    Ok(records)
}

fn local_tar_header_path(header: &[u8; 512]) -> String {
    let name = local_nul_trim(&header[0..100]);
    let prefix = local_nul_trim(&header[345..500]);
    if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    }
}

fn local_tar_header_linkname(header: &[u8; 512]) -> String {
    local_nul_trim(&header[157..257])
}

fn local_nul_trim(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).to_string()
}

fn tar_padding_len_local(size: u64) -> Result<u64, String> {
    round_up_512_local(size)?
        .checked_sub(size)
        .ok_or_else(|| "tar padding underflow".to_string())
}

fn round_up_512_local(value: u64) -> Result<u64, String> {
    value
        .checked_add(511)
        .map(|value| value / 512 * 512)
        .ok_or_else(|| "tar size overflows".to_string())
}

fn seek_forward<R: Seek>(reader: &mut R, bytes: u64) -> Result<(), String> {
    let delta = i64::try_from(bytes).map_err(|_| "tar seek distance is too large")?;
    reader
        .seek(SeekFrom::Current(delta))
        .map_err(|error| format!("seek tar payload: {error}"))?;
    Ok(())
}

fn archive_extract_range_request(
    args: &ArchiveExtractArgs,
) -> Result<(Option<&str>, Option<ArchiveByteRange>), String> {
    let has_range_metadata =
        args.path.is_some() || args.first_chunk_lba.is_some() || args.file_size_bytes.is_some();
    match (args.path.as_deref(), args.range) {
        (Some(path), Some(range)) => Ok((Some(path), Some(range))),
        (None, Some(_)) => Err("--range requires --path".to_string()),
        (Some(_), None) => {
            Err("--path, --first-chunk-lba, and --file-size-bytes require --range".to_string())
        }
        (_, None) if has_range_metadata => {
            Err("--path, --first-chunk-lba, and --file-size-bytes require --range".to_string())
        }
        (None, None) => Ok((None, None)),
    }
}

fn extract_plaintext_archive_range_file(
    args: &ArchiveExtractArgs,
    object: &RemTarReadObject,
    stored_size_bytes: u64,
    stored_size_blocks: u64,
    stored_digest: [u8; 32],
    member_path: &str,
    range: ArchiveByteRange,
) -> Result<Value, String> {
    let entry = object
        .entry(member_path)
        .ok_or_else(|| format!("plaintext RAO member {member_path:?} not found"))?;
    let entry = match entry.entry_type {
        RemTarEntryType::Regular => entry,
        RemTarEntryType::Hardlink => {
            let target = entry
                .link_target
                .as_deref()
                .ok_or_else(|| format!("hardlink member {member_path:?} is missing target"))?;
            object
                .entry(target)
                .filter(|target_entry| target_entry.entry_type == RemTarEntryType::Regular)
                .ok_or_else(|| {
                    format!(
                        "hardlink member {member_path:?} target {target:?} is not a regular primary"
                    )
                })?
        }
        _ => {
            return Err(format!(
                "range extraction requires a regular file, got {} for {member_path:?}",
                archive_entry_type_name(entry.entry_type)
            ));
        }
    };
    let range_end = validate_member_range(entry.size_bytes, range)?;
    let start = usize::try_from(range.start)
        .map_err(|_| "range start is too large for this host".to_string())?;
    let end = usize::try_from(range_end)
        .map_err(|_| "range end is too large for this host".to_string())?;
    let bytes = entry
        .data
        .get(start..end)
        .ok_or_else(|| "plaintext member bytes ended before validated range".to_string())?;
    let output = write_archive_range_output(&args.dest, member_path, bytes, args.overwrite)?;
    let context = ArchiveRangeExtractReportContext {
        object: &args.object,
        dest: &args.dest,
        output: &output,
        member_path,
        representation: "plaintext",
        encryption: "none",
        recipient_epochs: None,
        chunk_size: args.chunk_size,
        stored_size_bytes,
        stored_size_blocks,
        stored_digest,
        file_size_bytes: entry.size_bytes,
        first_chunk_lba: entry.first_chunk_lba.map(|lba| lba.0),
        range,
        bytes_written: bytes.len() as u64,
        authenticated_chunks: None,
        first_authenticated_chunk: None,
        stored_range_start: None,
        stored_range_len: None,
    };
    Ok(archive_range_extract_report_json(&context))
}

fn extract_encrypted_archive_range_file(
    args: &ArchiveExtractArgs,
    member_path: &str,
    range: ArchiveByteRange,
) -> Result<Value, String> {
    let file_size_bytes = args
        .file_size_bytes
        .ok_or_else(|| "encrypted RAO range extract requires --file-size-bytes".to_string())?;
    let first_chunk_lba = match args.first_chunk_lba {
        Some(lba) => Some(BodyLba(lba)),
        None if range.len == 0 => None,
        None => {
            return Err(
                "non-empty encrypted RAO range extract requires --first-chunk-lba".to_string(),
            )
        }
    };
    let encrypted = read_archive_object_bytes(&args.object)?;
    let inspected =
        inspect_bytes(&encrypted).map_err(|error| format!("inspect encrypted RAO: {error}"))?;
    let key = encrypted_archive_key(args)?;
    let range_result = read_encrypted_archive_file_range(
        &encrypted,
        &key,
        first_chunk_lba,
        file_size_bytes,
        range.start,
        range.len,
    )
    .map_err(|error| format!("extract encrypted RAO range: {error}"))?;
    let output =
        write_archive_range_output(&args.dest, member_path, &range_result.bytes, args.overwrite)?;
    let context = ArchiveRangeExtractReportContext {
        object: &args.object,
        dest: &args.dest,
        output: &output,
        member_path,
        representation: "encrypted",
        encryption: "RAO1",
        recipient_epochs: Some(recipient_epochs_json(&inspected.key_frame)),
        chunk_size: usize::try_from(inspected.header.chunk_size)
            .map_err(|_| "encrypted header chunk_size is too large for this host".to_string())?,
        stored_size_bytes: inspected.stored_size_bytes,
        stored_size_blocks: inspected.stored_size_bytes / u64::from(inspected.header.chunk_size),
        stored_digest: inspected.stored_digest,
        file_size_bytes,
        first_chunk_lba: first_chunk_lba.map(|lba| lba.0),
        range,
        bytes_written: range_result.bytes.len() as u64,
        authenticated_chunks: Some(range_result.envelope.chunk_count),
        first_authenticated_chunk: range_result.envelope.first_chunk,
        stored_range_start: range_result.envelope.stored_range_start,
        stored_range_len: Some(range_result.envelope.stored_range_len),
    };
    Ok(archive_range_extract_report_json(&context))
}

fn validate_member_range(file_size_bytes: u64, range: ArchiveByteRange) -> Result<u64, String> {
    let range_end = range
        .start
        .checked_add(range.len)
        .ok_or_else(|| "member range arithmetic overflow".to_string())?;
    if range.len == 0 {
        if range.start > file_size_bytes {
            return Err("empty member range starts past file end".to_string());
        }
    } else if range_end > file_size_bytes {
        return Err("member range extends past file end".to_string());
    }
    Ok(range_end)
}

fn read_plaintext_archive_object_file(
    path: &Path,
    chunk_size: usize,
) -> Result<(RemTarReadObject, u64, u64, [u8; 32]), String> {
    let mut source = FileBlockSource::open(path, chunk_size)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    let block_count = source.block_count();
    let stored_size_bytes = source.len_bytes();
    let object = read_rem_tar_object(&mut source, chunk_size, block_count)
        .map_err(|error| format!("read plaintext RAO: {error}"))?;
    let stored_digest = sha256_file(path)?;
    Ok((object, stored_size_bytes, block_count, stored_digest))
}

fn read_archive_object_bytes(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}

fn archive_object_is_encrypted(path: &Path) -> Result<bool, String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut magic = [0u8; 4];
    file.read_exact(&mut magic)
        .map_err(|error| format!("read {} magic: {error}", path.display()))?;
    Ok(&magic == b"RAO1")
}

fn plaintext_blocks_from_bytes(bytes: &[u8], chunk_size: usize) -> Result<Vec<Vec<u8>>, String> {
    if bytes.is_empty() {
        return Err("decrypted plaintext RAO is empty".to_string());
    }
    if bytes.len() % chunk_size != 0 {
        return Err(format!(
            "decrypted plaintext RAO size {} is not a multiple of chunk size {chunk_size}",
            bytes.len()
        ));
    }
    Ok(bytes
        .chunks_exact(chunk_size)
        .map(|chunk| chunk.to_vec())
        .collect())
}

fn plaintext_archive_inspect_json(
    path: &Path,
    object: &RemTarReadObject,
    chunk_size: usize,
    stored_size_bytes: u64,
    stored_size_blocks: u64,
    stored_digest: [u8; 32],
) -> Value {
    let manifest_sha256 = object.manifest_cbor.as_deref().map(sha256_bytes);
    json!({
        "representation": "plaintext",
        "encryption": "none",
        "object": path,
        "object_id": object.global_pax.get("REMANENCE.object_id"),
        "caller_object_id": object.global_pax.get("REMANENCE.caller_object_id"),
        "body_format": object.global_pax.get("REMANENCE.format_id"),
        "schema_version": object.global_pax.get("REMANENCE.schema_version"),
        "chunk_size": chunk_size,
        "stored_size_bytes": stored_size_bytes,
        "stored_size_blocks": stored_size_blocks,
        "stored_digest": bytes_to_hex(&stored_digest),
        "manifest_sha256": manifest_sha256.map(|hash| bytes_to_hex(&hash)),
        "files": object.entries.iter()
            .filter(|entry| entry.path != MANIFEST_PATH)
            .map(read_entry_report_json)
            .collect::<Vec<_>>(),
    })
}

fn encrypted_archive_keyless_json(path: &Path, report: &remanence_aead::InspectReport) -> Value {
    json!({
        "representation": "encrypted",
        "encryption": "RAO1",
        "keyed": false,
        "object": path,
        "object_id": report.header.object_id,
        "format_version": report.header.format_version,
        "chunk_size": report.header.chunk_size,
        "recipient_epochs": recipient_epochs_json(&report.key_frame),
        "hkdf_salt": bytes_to_hex(&report.header.hkdf_salt),
        "metadata_frame_len": report.header.metadata_frame_len,
        "stored_size_bytes": report.stored_size_bytes,
        "stored_size_blocks": report.stored_size_bytes / u64::from(report.header.chunk_size),
        "stored_digest": bytes_to_hex(&report.stored_digest),
        "plaintext_size_bytes": report.plaintext_size,
        "plaintext_chunk_count": report.chunk_count,
        "footer_offset": report.footer_offset,
    })
}

fn read_entry_report_json(entry: &remanence_format::RemTarReadEntry) -> Value {
    let mut report = json!({
        "entry_type": archive_entry_type_name(entry.entry_type),
        "path": entry.path,
        "size_bytes": entry.size_bytes,
        "file_id": entry.pax_records.get("REMANENCE.file_id"),
        "file_sha256": entry.pax_records.get("REMANENCE.file_sha256"),
        "link_target": entry.link_target,
        "first_chunk_lba": entry.first_chunk_lba.map(|lba| lba.0),
        "chunk_count": entry.chunk_count,
        "data_offset": entry.data_offset,
    });
    if let Some(xattrs) = xattrs_report_json(&entry.xattrs) {
        report
            .as_object_mut()
            .expect("entry report is a JSON object")
            .insert("xattrs".to_string(), json!(xattrs));
    }
    report
}

struct ArchiveExtractReportContext<'a> {
    object: &'a Path,
    dest: &'a Path,
    representation: &'static str,
    encryption: &'static str,
    recipient_epochs: Option<Vec<Value>>,
    chunk_size: usize,
    stored_size_bytes: u64,
    stored_size_blocks: u64,
    stored_digest: [u8; 32],
}

fn archive_extract_report_json(
    context: &ArchiveExtractReportContext<'_>,
    report: &remanence_stream::FilesystemRestoreReport,
) -> Value {
    json!({
        "object": context.object,
        "dest": context.dest,
        "representation": context.representation,
        "encryption": context.encryption,
        "recipient_epochs": context.recipient_epochs,
        "chunk_size": context.chunk_size,
        "stored_size_bytes": context.stored_size_bytes,
        "stored_size_blocks": context.stored_size_blocks,
        "stored_digest": bytes_to_hex(&context.stored_digest),
        "entries": report.stream.entries.len(),
        "files_written": report.files_written,
        "directories_seen": report.directories_seen,
        "symlinks_written": report.symlinks_written,
        "hardlinks_written": report.hardlinks_written,
        "bytes_written": report.bytes_written,
    })
}

struct ArchiveRangeExtractReportContext<'a> {
    object: &'a Path,
    dest: &'a Path,
    output: &'a Path,
    member_path: &'a str,
    representation: &'static str,
    encryption: &'static str,
    recipient_epochs: Option<Vec<Value>>,
    chunk_size: usize,
    stored_size_bytes: u64,
    stored_size_blocks: u64,
    stored_digest: [u8; 32],
    file_size_bytes: u64,
    first_chunk_lba: Option<u64>,
    range: ArchiveByteRange,
    bytes_written: u64,
    authenticated_chunks: Option<u64>,
    first_authenticated_chunk: Option<u64>,
    stored_range_start: Option<u64>,
    stored_range_len: Option<u64>,
}

fn archive_range_extract_report_json(context: &ArchiveRangeExtractReportContext<'_>) -> Value {
    json!({
        "mode": "range",
        "object": context.object,
        "dest": context.dest,
        "output": context.output,
        "path": context.member_path,
        "representation": context.representation,
        "encryption": context.encryption,
        "recipient_epochs": context.recipient_epochs,
        "chunk_size": context.chunk_size,
        "stored_size_bytes": context.stored_size_bytes,
        "stored_size_blocks": context.stored_size_blocks,
        "stored_digest": bytes_to_hex(&context.stored_digest),
        "file_size_bytes": context.file_size_bytes,
        "first_chunk_lba": context.first_chunk_lba,
        "range_start": context.range.start,
        "range_len": context.range.len,
        "bytes_written": context.bytes_written,
        "authenticated_chunks": context.authenticated_chunks,
        "first_authenticated_chunk": context.first_authenticated_chunk,
        "stored_range_start": context.stored_range_start,
        "stored_range_len": context.stored_range_len,
    })
}

fn write_archive_range_output(
    root: &Path,
    member_path: &str,
    bytes: &[u8],
    overwrite: bool,
) -> Result<PathBuf, String> {
    let destination = archive_range_destination(root, member_path)?;
    write_bytes_to_archive_output(&destination, bytes, overwrite)?;
    Ok(destination)
}

fn write_blob_member_output(
    root: &Path,
    member_path: &str,
    bytes: &[u8],
    overwrite: bool,
) -> Result<PathBuf, String> {
    let destination = blob_member_destination(root, member_path)?;
    write_bytes_to_archive_output(&destination, bytes, overwrite)?;
    Ok(destination)
}

fn write_bytes_to_archive_output(
    destination: &Path,
    bytes: &[u8],
    overwrite: bool,
) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options
        .open(destination)
        .map_err(|error| format!("open range output {}: {error}", destination.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("write range output {}: {error}", destination.display()))
}

fn archive_range_destination(root: &Path, member_path: &str) -> Result<PathBuf, String> {
    ensure_archive_extract_root(root)?;
    let parts = archive_member_path_parts(member_path)?;
    let mut destination = root.to_path_buf();
    for part in &parts[..parts.len().saturating_sub(1)] {
        destination.push(part);
        ensure_archive_extract_directory(&destination)?;
    }
    destination.push(parts.last().expect("member path has at least one part"));
    reject_archive_extract_symlink(&destination)?;
    Ok(destination)
}

fn blob_member_destination(root: &Path, member_path: &str) -> Result<PathBuf, String> {
    ensure_archive_extract_root(root)?;
    let decoded = archive_ingest::decode_member_name(member_path)
        .map_err(|error| format!("decode blob member path {member_path:?}: {error}"))?;
    let parts = archive_member_path_byte_parts(member_path, &decoded)?;
    let mut destination = root.to_path_buf();
    for part in &parts[..parts.len().saturating_sub(1)] {
        push_raw_path_component(&mut destination, part)?;
        ensure_archive_extract_directory(&destination)?;
    }
    push_raw_path_component(
        &mut destination,
        parts.last().expect("member path has at least one part"),
    )?;
    reject_archive_extract_symlink(&destination)?;
    Ok(destination)
}

fn archive_member_path_byte_parts<'a>(
    member_path: &str,
    decoded: &'a [u8],
) -> Result<Vec<&'a [u8]>, String> {
    if decoded.is_empty() || decoded.ends_with(b"/") {
        return Err("blob-member extraction requires a regular archive file path".to_string());
    }
    let mut parts = Vec::new();
    for part in decoded.split(|byte| *byte == b'/') {
        if part.is_empty() || part == b"." || part == b".." || part.contains(&0) {
            return Err(format!(
                "blob-member path {member_path:?} is not a normalized relative path"
            ));
        }
        parts.push(part);
    }
    Ok(parts)
}

#[cfg(unix)]
fn push_raw_path_component(path: &mut PathBuf, part: &[u8]) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;

    if part.contains(&0) {
        return Err("blob-member path component contains NUL".to_string());
    }
    path.push(OsStr::from_bytes(part));
    Ok(())
}

#[cfg(not(unix))]
fn push_raw_path_component(path: &mut PathBuf, part: &[u8]) -> Result<(), String> {
    let text = std::str::from_utf8(part).map_err(|error| {
        format!("blob-member path is not representable on this platform: {error}")
    })?;
    path.push(text);
    Ok(())
}

fn archive_member_path_parts(member_path: &str) -> Result<Vec<&str>, String> {
    if member_path.is_empty() || member_path.ends_with('/') {
        return Err("range extraction requires a regular archive file path".to_string());
    }
    let mut parts = Vec::new();
    for part in member_path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.as_bytes().contains(&0) {
            return Err(format!(
                "range extraction path {member_path:?} is not a normalized relative path"
            ));
        }
        parts.push(part);
    }
    Ok(parts)
}

fn ensure_archive_extract_root(root: &Path) -> Result<(), String> {
    fs::create_dir_all(root)
        .map_err(|error| format!("create extract root {}: {error}", root.display()))?;
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("inspect extract root {}: {error}", root.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "extract root must not be a symlink: {}",
            root.display()
        ));
    }
    if !metadata.is_dir() {
        return Err(format!(
            "extract root must be a directory: {}",
            root.display()
        ));
    }
    Ok(())
}

fn ensure_archive_extract_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "range output parent escapes destination through a symlink: {}",
            path.display()
        )),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "range output parent exists but is not a directory: {}",
            path.display()
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| {
                format!("create range output directory {}: {error}", path.display())
            })?;
            let metadata = fs::symlink_metadata(path).map_err(|error| {
                format!("inspect range output directory {}: {error}", path.display())
            })?;
            if metadata.file_type().is_symlink() {
                return Err(format!(
                    "range output parent escapes destination through a symlink: {}",
                    path.display()
                ));
            }
            Ok(())
        }
        Err(error) => Err(format!(
            "inspect range output directory {}: {error}",
            path.display()
        )),
    }
}

fn reject_archive_extract_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "range output escapes destination through a symlink: {}",
            path.display()
        )),
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(format!(
            "range output exists but is not a regular file: {}",
            path.display()
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect range output {}: {error}", path.display())),
    }
}

fn required_global_pax<'a>(object: &'a RemTarReadObject, key: &str) -> Result<&'a str, String> {
    object
        .global_pax
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("decrypted RAO is missing global pax key {key}"))
}

struct ArchiveBuildResult {
    layout: RemTarObjectLayout,
    representation: &'static str,
    encryption: &'static str,
    format_version: Option<u8>,
    recipient_epochs: Option<Vec<Value>>,
    stored_digest: [u8; 32],
    plaintext_digest: [u8; 32],
    stored_size_bytes: u64,
    stored_size_blocks: u64,
}

fn archive_build_report_json(
    args: &ArchiveBuildArgs,
    inputs: &[ArchiveBuildInputFile],
    build: &ArchiveBuildResult,
    ingest: Option<&archive_ingest::MaterializedArchiveInputs>,
    map_sha256: Option<[u8; 32]>,
) -> Value {
    let files: Vec<Value> = build
        .layout
        .files
        .iter()
        .zip(inputs.iter())
        .map(|(layout, input)| archive_file_report_json(layout, input))
        .collect();

    let mut report = json!({
        "object_id": build.layout.object_id,
        "caller_object_id": build.layout.caller_object_id,
        "body_format": FORMAT_ID,
        "representation": build.representation,
        "encryption": build.encryption,
        "format_version": build.format_version,
        "recipient_epochs": build.recipient_epochs,
        "chunk_size": build.layout.chunk_size,
        "stored_size_bytes": build.stored_size_bytes,
        "stored_size_blocks": build.stored_size_blocks,
        "stored_digest": bytes_to_hex(&build.stored_digest),
        "plaintext_digest": bytes_to_hex(&build.plaintext_digest),
        "manifest_sha256": bytes_to_hex(&build.layout.manifest_sha256),
        "out": args.out,
        "files": files,
    });
    if let Some(ingest) = ingest {
        report["ingest"] = serde_json::to_value(&ingest.report)
            .unwrap_or_else(|error| json!({ "error": error.to_string() }));
    }
    if let Some(map_sha256) = map_sha256 {
        report["map_sha256"] = json!(bytes_to_hex(&map_sha256));
    }
    if let Some(path) = &args.manifest_out {
        report["manifest_out"] = json!(path);
    }
    report
}

fn archive_file_report_json(layout: &RemTarFileLayout, input: &ArchiveBuildInputFile) -> Value {
    let mut report = json!({
        "entry_type": archive_entry_type_name(layout.entry_type),
        "path": layout.path,
        "source_path": input.source_path,
        "file_id": layout.file_id,
        "size_bytes": layout.size_bytes,
        "file_sha256": input.file_sha256.map(|hash| bytes_to_hex(&hash)),
        "link_target": layout.link_target,
        "pax_header_offset": layout.pax_header_offset,
        "data_offset": layout.data_offset,
        "first_chunk_lba": layout.first_chunk_lba.map(|lba| lba.0),
        "chunk_count": layout.chunk_count,
        "pad_spaces": layout.pad_spaces,
    });
    if let Some(xattrs) = xattrs_report_json(&layout.xattrs) {
        report
            .as_object_mut()
            .expect("archive file report is a JSON object")
            .insert("xattrs".to_string(), json!(xattrs));
    }
    if let Some(ingest_item_id) = &input.ingest_item_id {
        report
            .as_object_mut()
            .expect("archive file report is a JSON object")
            .insert("ingest_item_id".to_string(), json!(ingest_item_id));
    }
    report
}

fn xattrs_report_json(xattrs: &BTreeMap<String, Vec<u8>>) -> Option<BTreeMap<String, String>> {
    if xattrs.is_empty() {
        return None;
    }
    Some(
        xattrs
            .iter()
            .map(|(name, value)| (name.clone(), bytes_to_hex(value)))
            .collect(),
    )
}

fn archive_entry_type_name(entry_type: RemTarEntryType) -> &'static str {
    match entry_type {
        RemTarEntryType::Regular => "regular",
        RemTarEntryType::Hardlink => "hardlink",
        RemTarEntryType::Symlink => "symlink",
        RemTarEntryType::Directory => "directory",
    }
}

fn collect_archive_build_inputs(paths: &[PathBuf]) -> Result<Vec<ArchiveBuildInputFile>, String> {
    let mut files = Vec::new();
    for path in paths {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("stat input {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            let name = path.file_name().ok_or_else(|| {
                format!("input symlink {} does not have a file name", path.display())
            })?;
            let archive_path = path_component_to_string(name)?;
            files.push(read_archive_build_symlink(path, archive_path)?);
        } else if metadata.is_dir() {
            let added = collect_archive_build_dir(path, path, &mut files)?;
            if !added {
                let name = path.file_name().ok_or_else(|| {
                    format!(
                        "input directory {} does not have a file name",
                        path.display()
                    )
                })?;
                let archive_path = format!("{}/", path_component_to_string(name)?);
                files.push(read_archive_build_directory(path, archive_path)?);
            }
        } else if metadata.is_file() {
            let name = path.file_name().ok_or_else(|| {
                format!("input file {} does not have a file name", path.display())
            })?;
            let archive_path = path_component_to_string(name)?;
            files.push(read_archive_build_file(path, archive_path)?);
        } else {
            return Err(format!(
                "input {} is not a regular file, symlink, or directory",
                path.display()
            ));
        }
    }
    files.sort_by(|a, b| a.archive_path.cmp(&b.archive_path));
    let mut seen = std::collections::BTreeSet::new();
    for file in &files {
        if !seen.insert(file.archive_path.clone()) {
            return Err(format!("duplicate archive path {:?}", file.archive_path));
        }
    }
    Ok(files)
}

fn collect_archive_build_dir(
    root: &Path,
    dir: &Path,
    files: &mut Vec<ArchiveBuildInputFile>,
) -> Result<bool, String> {
    let mut entries = std::fs::read_dir(dir)
        .map_err(|error| format!("read directory {}: {error}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("read directory {}: {error}", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());
    if entries.is_empty() {
        let relative = dir
            .strip_prefix(root)
            .map_err(|error| format!("derive archive path for {}: {error}", dir.display()))?;
        if !relative.as_os_str().is_empty() {
            let archive_path = format!("{}/", archive_path_from_relative(relative)?);
            files.push(read_archive_build_directory(dir, archive_path)?);
            return Ok(true);
        }
        return Ok(false);
    }
    let mut added_any = false;
    for entry in entries {
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("stat input {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("derive archive path for {}: {error}", path.display()))?;
            let archive_path = archive_path_from_relative(relative)?;
            files.push(read_archive_build_symlink(&path, archive_path)?);
            added_any = true;
        } else if metadata.is_dir() {
            if collect_archive_build_dir(root, &path, files)? {
                added_any = true;
            }
        } else if metadata.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("derive archive path for {}: {error}", path.display()))?;
            let archive_path = archive_path_from_relative(relative)?;
            files.push(read_archive_build_file(&path, archive_path)?);
            added_any = true;
        } else {
            return Err(format!(
                "input {} is not a regular file, symlink, or directory",
                path.display()
            ));
        }
    }
    Ok(added_any)
}

fn read_archive_build_file(
    source_path: &Path,
    archive_path: String,
) -> Result<ArchiveBuildInputFile, String> {
    read_archive_build_file_with_xattrs(source_path, archive_path, BTreeMap::new())
}

fn read_archive_build_file_with_xattrs(
    source_path: &Path,
    archive_path: String,
    xattrs: BTreeMap<String, Vec<u8>>,
) -> Result<ArchiveBuildInputFile, String> {
    let (size_bytes, file_sha256) = hash_archive_build_file(source_path)?;
    let file_id = deterministic_archive_entry_file_id(
        RemTarEntryType::Regular,
        &archive_path,
        Some(&file_sha256),
        None,
    );
    Ok(ArchiveBuildInputFile {
        source_path: source_path.to_path_buf(),
        entry_type: RemTarEntryType::Regular,
        archive_path,
        file_id,
        size_bytes,
        file_sha256: Some(file_sha256),
        link_target: None,
        xattrs,
        ingest_item_id: None,
    })
}

fn read_archive_build_hardlink(
    source_path: &Path,
    archive_path: String,
    link_target: String,
) -> Result<ArchiveBuildInputFile, String> {
    let file_id = deterministic_archive_entry_file_id(
        RemTarEntryType::Hardlink,
        &archive_path,
        None,
        Some(&link_target),
    );
    Ok(ArchiveBuildInputFile {
        source_path: source_path.to_path_buf(),
        entry_type: RemTarEntryType::Hardlink,
        archive_path,
        file_id,
        size_bytes: 0,
        file_sha256: None,
        link_target: Some(link_target),
        xattrs: BTreeMap::new(),
        ingest_item_id: None,
    })
}

fn read_archive_build_symlink(
    source_path: &Path,
    archive_path: String,
) -> Result<ArchiveBuildInputFile, String> {
    let target = std::fs::read_link(source_path)
        .map_err(|error| format!("read symlink {}: {error}", source_path.display()))?;
    let target = target
        .to_str()
        .ok_or_else(|| format!("symlink target for {} must be UTF-8", source_path.display()))?
        .to_string();
    let file_id = deterministic_archive_entry_file_id(
        RemTarEntryType::Symlink,
        &archive_path,
        None,
        Some(&target),
    );
    Ok(ArchiveBuildInputFile {
        source_path: source_path.to_path_buf(),
        entry_type: RemTarEntryType::Symlink,
        archive_path,
        file_id,
        size_bytes: 0,
        file_sha256: None,
        link_target: Some(target),
        xattrs: BTreeMap::new(),
        ingest_item_id: None,
    })
}

fn read_archive_build_directory(
    source_path: &Path,
    archive_path: String,
) -> Result<ArchiveBuildInputFile, String> {
    let file_id =
        deterministic_archive_entry_file_id(RemTarEntryType::Directory, &archive_path, None, None);
    Ok(ArchiveBuildInputFile {
        source_path: source_path.to_path_buf(),
        entry_type: RemTarEntryType::Directory,
        archive_path,
        file_id,
        size_bytes: 0,
        file_sha256: None,
        link_target: None,
        xattrs: BTreeMap::new(),
        ingest_item_id: None,
    })
}

fn archive_build_file_spec(input: &ArchiveBuildInputFile) -> RemTarFileSpec {
    match input.entry_type {
        RemTarEntryType::Regular => {
            let mut spec = RemTarFileSpec::new(
                input.archive_path.clone(),
                input.file_id.clone(),
                input.size_bytes,
                input.file_sha256.expect("regular input has sha256"),
            );
            spec.executable = Some(false);
            spec.xattrs = input.xattrs.clone();
            spec
        }
        RemTarEntryType::Hardlink => RemTarFileSpec::hardlink(
            input.archive_path.clone(),
            input.file_id.clone(),
            input
                .link_target
                .clone()
                .expect("hardlink input has link target"),
        ),
        RemTarEntryType::Symlink => RemTarFileSpec::symlink(
            input.archive_path.clone(),
            input.file_id.clone(),
            input
                .link_target
                .clone()
                .expect("symlink input has link target"),
        ),
        RemTarEntryType::Directory => {
            RemTarFileSpec::directory(input.archive_path.clone(), input.file_id.clone())
        }
    }
}

fn archive_build_streams<'a>(
    inputs: &[ArchiveBuildInputFile],
    readers: &'a mut [Box<dyn Read>],
) -> Vec<RemTarFileStream<'a>> {
    inputs
        .iter()
        .zip(readers.iter_mut())
        .map(|(input, reader)| {
            RemTarFileStream::new(archive_build_file_spec(input), reader.as_mut())
        })
        .collect()
}

fn open_archive_build_readers(
    inputs: &[ArchiveBuildInputFile],
) -> Result<Vec<Box<dyn Read>>, String> {
    inputs
        .iter()
        .map(|input| {
            if input.entry_type == RemTarEntryType::Regular {
                let file = File::open(&input.source_path).map_err(|error| {
                    format!("open input {}: {error}", input.source_path.display())
                })?;
                let size = file
                    .metadata()
                    .map_err(|error| {
                        format!("stat input {}: {error}", input.source_path.display())
                    })?
                    .len();
                if size != input.size_bytes {
                    return Err(format!(
                        "input {} changed size while building: expected {}, got {}",
                        input.source_path.display(),
                        input.size_bytes,
                        size
                    ));
                }
                Ok(Box::new(file) as Box<dyn Read>)
            } else {
                Ok(Box::new(io::empty()) as Box<dyn Read>)
            }
        })
        .collect()
}

fn hash_archive_build_file(path: &Path) -> Result<(u64, [u8; 32]), String> {
    let mut file =
        File::open(path).map_err(|error| format!("open input {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read input {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| format!("input {} is too large", path.display()))?;
        hasher.update(&buffer[..read]);
    }
    Ok((size, finalize_sha256(hasher)))
}

fn archive_path_from_relative(path: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(path_component_to_string(part)?),
            _ => {
                return Err(format!(
                    "unsupported archive path component in {}",
                    path.display()
                ))
            }
        }
    }
    if parts.is_empty() {
        return Err("archive path must not be empty".to_string());
    }
    Ok(parts.join("/"))
}

fn path_component_to_string(part: &std::ffi::OsStr) -> Result<String, String> {
    let value = part
        .to_str()
        .ok_or_else(|| "archive path component must be UTF-8".to_string())?;
    if value.is_empty() || value == "." || value == ".." || value.contains('/') {
        return Err(format!("invalid archive path component {value:?}"));
    }
    Ok(value.to_string())
}

fn temporary_archive_output_path(out: &Path) -> PathBuf {
    let file_name = out
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("archive.rao");
    let tmp_name = format!(".{file_name}.tmp.{}", std::process::id());
    out.with_file_name(tmp_name)
}

/// Plaintext staging file that is truncated before its directory entry is removed.
struct SecurePlaintextStage(tempfile::NamedTempFile);

impl SecurePlaintextStage {
    fn new_in(directory: &Path) -> Result<Self, String> {
        tempfile::Builder::new()
            .prefix(".rao-plaintext.")
            .tempfile_in(directory)
            .map(Self)
            .map_err(|error| {
                format!(
                    "create secure plaintext staging file in {}: {error}",
                    directory.display()
                )
            })
    }

    fn as_file_mut(&mut self) -> &mut File {
        self.0.as_file_mut()
    }
}

impl Drop for SecurePlaintextStage {
    fn drop(&mut self) {
        // Best effort only: filesystems and storage devices may retain old
        // blocks, but truncation avoids leaving an intact plaintext file.
        let _ = self.0.as_file_mut().set_len(0);
        let _ = self.0.as_file_mut().sync_all();
    }
}

fn read_private_key_file(path: &Path) -> Result<remanence_aead::RecipientPrivateKey, String> {
    let mut bytes = std::fs::read(path)
        .map_err(|error| format!("read --private-key {}: {error}", path.display()))?;
    let parsed = remanence_aead::RecipientPrivateKey::parse(&bytes)
        .map_err(|error| format!("parse --private-key {}: {error}", path.display()));
    bytes.zeroize();
    parsed
}

fn read_recipient_public_key_files(paths: &[PathBuf]) -> Result<Vec<RecipientPublicKey>, String> {
    if !(2..=8).contains(&paths.len()) {
        return Err("--recipient must be repeated 2 to 8 times".to_string());
    }
    let mut recipients = Vec::with_capacity(paths.len());
    for path in paths {
        let bytes = fs::read(path)
            .map_err(|error| format!("read --recipient {}: {error}", path.display()))?;
        recipients.push(
            RecipientPublicKey::parse(&bytes)
                .map_err(|error| format!("parse --recipient {}: {error}", path.display()))?,
        );
    }
    if recipients
        .windows(2)
        .any(|pair| pair[0].slot_index >= pair[1].slot_index)
        || recipients.iter().enumerate().any(|(index, recipient)| {
            recipients[..index]
                .iter()
                .any(|earlier| earlier.recipient_epoch_id == recipient.recipient_epoch_id)
        })
    {
        return Err("--recipient epochs must be distinct and in ascending slot order".to_string());
    }
    Ok(recipients)
}

fn recipient_epochs_json(key_frame: &remanence_aead::KeyFrame) -> Vec<Value> {
    key_frame
        .slots
        .iter()
        .map(|slot| {
            json!({
                "epoch_id": bytes_to_hex(&slot.recipient_epoch_id),
                "label": slot.epoch_label,
            })
        })
        .collect()
}

fn sha256_file(path: &Path) -> Result<[u8; 32], String> {
    let mut file = File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(finalize_sha256(hasher))
}

fn sha256_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    finalize_sha256(hasher)
}

fn deterministic_archive_entry_file_id(
    entry_type: RemTarEntryType,
    archive_path: &str,
    file_sha256: Option<&[u8; 32]>,
    link_target: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"remanence-cli archive build entry-id v1\0");
    hasher.update(archive_entry_type_name(entry_type).as_bytes());
    hasher.update(b"\0");
    hasher.update(archive_path.as_bytes());
    hasher.update(b"\0");
    if let Some(file_sha256) = file_sha256 {
        hasher.update(file_sha256);
    }
    hasher.update(b"\0");
    if let Some(link_target) = link_target {
        hasher.update(link_target.as_bytes());
    }
    bytes_to_hex(&finalize_sha256(hasher))
}

fn finalize_sha256(hasher: Sha256) -> [u8; 32] {
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

fn run_archive_tape_command(
    report: &DiscoveryReport,
    command: &ArchiveCommand,
    allow: &[String],
    allow_derived: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let serial = match command.source().selection() {
        Ok(ArchiveSourceSelection::Tape { serial, .. }) => serial,
        Ok(ArchiveSourceSelection::Dump(_)) => {
            let _ = writeln!(err, "error: internal dispatch bug for archive dump source");
            return ExitCode::from(1);
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            return ExitCode::from(1);
        }
    };
    let lib = match report.library(serial) {
        Some(l) => l,
        None => {
            let _ = writeln!(err, "error: no library with serial {serial:?} on this host");
            let _ = writeln!(err, "       run `rem libraries` to see what's available");
            print_warnings(report, err);
            return ExitCode::from(2);
        }
    };
    let mut policy = StaticAllowlist::new(allow.iter().cloned());
    for s in allow_derived {
        policy = policy.with_derived_allowed(s.clone());
    }
    open_and_run_archive_tape(lib, command, &policy, out, err, report)
}

#[cfg(target_os = "linux")]
fn open_and_run_archive_tape(
    lib: &Library,
    command: &ArchiveCommand,
    policy: &dyn remanence_library::AccessPolicy,
    out: &mut dyn Write,
    err: &mut dyn Write,
    report: &DiscoveryReport,
) -> ExitCode {
    let mut handle = match open_library_handle(lib, policy) {
        Ok(h) => h,
        Err(e) => {
            let s = e.to_string();
            let _ = writeln!(err, "error: opening library: {s}");
            print_setcap_hint_if_error_text_matches(&s, err);
            print_warnings(report, err);
            return ExitCode::from(1);
        }
    };

    let result: Result<(), String> = (|| {
        let (bay, rewind) = match command.source().selection() {
            Ok(ArchiveSourceSelection::Tape { bay, rewind, .. }) => (bay, rewind),
            Ok(ArchiveSourceSelection::Dump(_)) => unreachable!("dump source reached tape runner"),
            Err(error) => return Err(error),
        };
        let mut drive = handle
            .open_drive(bay, policy)
            .map_err(|error| error.to_string())?;
        if rewind {
            drive.rewind().map_err(|error| error.to_string())?;
        }
        run_archive_tape_with_drive(command, &mut drive, out, err)
    })();

    match result {
        Ok(()) => {
            if let Some(cause) = handle.dirty_cause() {
                let library_serial = handle.library().serial.clone();
                print_dirty_snapshot_recovery(&library_serial, DirtyReason::from(cause), err);
            }
            print_warnings(report, err);
            ExitCode::SUCCESS
        }
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            print_setcap_hint_if_error_text_matches(&error, err);
            if let Some(cause) = handle.dirty_cause() {
                let library_serial = handle.library().serial.clone();
                print_dirty_snapshot_recovery(&library_serial, DirtyReason::from(cause), err);
            }
            print_warnings(report, err);
            ExitCode::from(1)
        }
    }
}

#[cfg(target_os = "linux")]
fn run_archive_tape_with_drive(
    command: &ArchiveCommand,
    drive: &mut remanence_library::DriveHandle,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), String> {
    let format = command.format();
    let mut source = DriveHandlePhysicalSource::new(drive);
    let probe = probe_tape_archive(format, &mut source).map_err(|error| error.to_string())?;
    match command {
        ArchiveCommand::Probe(_) => {
            print_probe(&probe, out);
            Ok(())
        }
        ArchiveCommand::Scan(_) => {
            ensure_probe_matches(format, &probe)?;
            let mut archive = open_tape_archive(format, &mut source, &probe)
                .map_err(|error| error.to_string())?;
            let report = remanence_stream::scan_archive_reader(archive.as_mut())
                .map_err(|error| error.to_string())?;
            print_archive_scan(&report, out);
            Ok(())
        }
        ArchiveCommand::Restore(args) => {
            ensure_probe_matches(format, &probe)?;
            let mut archive = open_tape_archive(format, &mut source, &probe)
                .map_err(|error| error.to_string())?;
            let report = remanence_stream::restore_archive_reader_to_directory(
                archive.as_mut(),
                &args.dest,
                remanence_stream::FilesystemRestoreOptions {
                    overwrite: args.overwrite,
                    include_manifest: false,
                },
            )
            .map_err(|error| error.to_string())?;
            print_archive_restore(&report, out);
            print_damage_list(&report.damages, err);
            print_archive_gap_list(&report.archive_gaps, err);
            Ok(())
        }
        ArchiveCommand::Recover(args) => {
            ensure_probe_matches(format, &probe)?;
            let mut archive = open_tape_archive(format, &mut source, &probe)
                .map_err(|error| error.to_string())?;
            let source_description = match args.source.selection() {
                Ok(ArchiveSourceSelection::Tape { serial, bay, .. }) => {
                    format!("tape:{serial}:bay:{bay}")
                }
                Ok(ArchiveSourceSelection::Dump(_)) => "tape:<internal-dispatch-error>".to_string(),
                Err(error) => return Err(error),
            };
            let report = remanence_stream::recover_archive_reader_to_directory(
                archive.as_mut(),
                &args.dest,
                remanence_stream::RecoveryOptions::new(format.driver_id(), source_description),
            )
            .map_err(|error| error.to_string())?;
            print_archive_recovery(&report, out);
            print_damage_list_from_recovery(&report.files, err);
            print_recovery_archive_gap_list(&report.archive_gaps, err);
            Ok(())
        }
        ArchiveCommand::Build(_) => {
            unreachable!("archive build dispatched before the tape archive handler")
        }
        ArchiveCommand::Capabilities => {
            unreachable!("archive capabilities dispatched before the tape archive handler")
        }
        ArchiveCommand::Reseal(_) => {
            unreachable!("archive reseal dispatched before the tape archive handler")
        }
        ArchiveCommand::Inspect(_) => {
            unreachable!("archive inspect dispatched before the tape archive handler")
        }
        ArchiveCommand::Extract(_) => {
            unreachable!("archive extract dispatched before the tape archive handler")
        }
        ArchiveCommand::ExtractStream(_) => {
            unreachable!("archive extract-stream dispatched before the tape archive handler")
        }
        ArchiveCommand::CoveringRange(_) => {
            unreachable!("archive covering-range dispatched before the tape archive handler")
        }
        ArchiveCommand::Write(_) => {
            unreachable!("archive write dispatched before the tape archive handler")
        }
        ArchiveCommand::Read(_) => {
            unreachable!("archive read dispatched before the tape archive handler")
        }
        ArchiveCommand::ExportObject(_) => {
            unreachable!("archive export-object dispatched before the tape archive handler")
        }
        ArchiveCommand::Verify(_) => {
            unreachable!("archive verify dispatched before the tape archive handler")
        }
        ArchiveCommand::List(_) => {
            unreachable!("archive list dispatched before the tape archive handler")
        }
    }
}

#[cfg(target_os = "linux")]
fn probe_tape_archive(
    format: ArchiveFormat,
    source: &mut dyn remanence_library::PhysicalTapeSource,
) -> Result<ProbeResult, FormatError> {
    match format {
        #[cfg(feature = "foreign-bru")]
        ArchiveFormat::Bru => BruFormat.probe(source),
        #[cfg(not(feature = "foreign-bru"))]
        ArchiveFormat::Bru => {
            let _ = source;
            Err(format_unavailable_error(format))
        }
    }
}

#[cfg(target_os = "linux")]
fn open_tape_archive<'a>(
    format: ArchiveFormat,
    source: &'a mut dyn remanence_library::PhysicalTapeSource,
    probe: &ProbeResult,
) -> Result<Box<dyn ArchiveReader + 'a>, FormatError> {
    match format {
        #[cfg(feature = "foreign-bru")]
        ArchiveFormat::Bru => BruFormat.open_tape_reader(source, probe),
        #[cfg(not(feature = "foreign-bru"))]
        ArchiveFormat::Bru => {
            let _ = source;
            let _ = probe;
            Err(format_unavailable_error(format))
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn open_and_run_archive_tape(
    _lib: &Library,
    _command: &ArchiveCommand,
    _policy: &dyn remanence_library::AccessPolicy,
    _out: &mut dyn Write,
    err: &mut dyn Write,
    report: &DiscoveryReport,
) -> ExitCode {
    let _ = writeln!(
        err,
        "error: archive tape commands require Linux (drive access is Linux-only in v0.1)"
    );
    print_warnings(report, err);
    ExitCode::from(1)
}

fn ensure_probe_matches(format: ArchiveFormat, probe: &ProbeResult) -> Result<(), String> {
    match probe.confidence {
        ProbeConfidence::NoMatch => Err(format!(
            "{} probe returned no match at this tape position",
            format.cli_name()
        )),
        ProbeConfidence::Possible | ProbeConfidence::Probable | ProbeConfidence::Certain => Ok(()),
    }
}

/// Parse a duration string accepted by `rem watch --coalesce-window`.
/// Forms: `<number>ms`, `<number>s`, or `0` (meaning disabled).
#[cfg(all(target_os = "linux", feature = "linux-udev"))]
fn parse_duration_arg(s: &str) -> Result<std::time::Duration, String> {
    let t = s.trim();
    if t == "0" {
        return Ok(std::time::Duration::ZERO);
    }
    if let Some(ms) = t.strip_suffix("ms") {
        let n: u64 = ms
            .trim()
            .parse()
            .map_err(|e| format!("invalid duration {s:?}: {e}"))?;
        return Ok(std::time::Duration::from_millis(n));
    }
    if let Some(sec) = t.strip_suffix('s') {
        let n: u64 = sec
            .trim()
            .parse()
            .map_err(|e| format!("invalid duration {s:?}: {e}"))?;
        return Ok(std::time::Duration::from_secs(n));
    }
    Err(format!(
        "invalid duration {s:?}: expected '<number>ms', '<number>s', or '0'"
    ))
}

// ===================================================================
//  `rem watch` — hot-plug burst pretty-printer.
//
//  Two implementations: the real one behind `linux-udev`, and a
//  graceful "feature not built in" stub for every other build path.
// ===================================================================

#[cfg(all(target_os = "linux", feature = "linux-udev"))]
fn run_watch(coalesce_window: &str, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    use remanence_library::watch::{HotplugSource, LinuxUdevSource, WatcherError};

    let window = match parse_duration_arg(coalesce_window) {
        Ok(d) => d,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            return ExitCode::from(1);
        }
    };

    let _ = writeln!(
        out,
        "rem watch — listening for SCSI hot-plug events (coalesce window {window:?})\n\
         Ctrl-C to exit; bursts print one per line."
    );

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            let _ = writeln!(err, "error: failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async move {
        let mut source = match LinuxUdevSource::new() {
            Ok(s) => s,
            Err(WatcherError::SourceUnavailable(msg)) => {
                let _ = writeln!(err, "error: udev source unavailable: {msg}");
                let _ = writeln!(
                    err,
                    "       are you running inside a container without udev passthrough?"
                );
                return ExitCode::from(1);
            }
            Err(e) => {
                let _ = writeln!(err, "error: {e}");
                return ExitCode::from(1);
            }
        };
        source.set_coalesce_window(window);

        let mut rx = match source.subscribe() {
            Ok(rx) => rx,
            Err(e) => {
                let _ = writeln!(err, "error: subscribe failed: {e}");
                return ExitCode::from(1);
            }
        };

        let start = std::time::Instant::now();
        let mut burst_seq: u64 = 0;
        while let Some(item) = rx.recv().await {
            match item {
                Ok(burst) => {
                    burst_seq += 1;
                    let elapsed = start.elapsed();
                    let span = burst.last_at.saturating_duration_since(burst.first_at);
                    let subs: Vec<String> =
                        burst.subsystems.iter().map(|s| format!("{s:?}")).collect();
                    let kinds: Vec<String> = burst.kinds.iter().map(|k| format!("{k:?}")).collect();
                    let path_count = burst.touched_paths.len();
                    let _ = writeln!(
                        out,
                        "[{elapsed:>10.3?}] burst #{burst_seq}: events={count} span={span:?} \
                         subsystems=[{subs}] kinds=[{kinds}] paths={path_count} \
                         unknown_scope={unknown}",
                        count = burst.raw_event_count,
                        subs = subs.join(","),
                        kinds = kinds.join(","),
                        unknown = burst.has_unknown_scope,
                    );
                    for p in &burst.touched_paths {
                        let _ = writeln!(out, "             {}", p.display());
                    }
                }
                Err(WatcherError::SourceClosed) => {
                    let _ = writeln!(err, "udev source closed — exiting");
                    return ExitCode::from(0);
                }
                Err(e) => {
                    let _ = writeln!(err, "watcher error: {e}");
                    return ExitCode::from(1);
                }
            }
        }
        ExitCode::SUCCESS
    })
}

#[cfg(not(all(target_os = "linux", feature = "linux-udev")))]
fn run_watch(_coalesce_window: &str, _out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let _ = writeln!(
        err,
        "error: `rem watch` requires the `linux-udev` build feature."
    );
    let _ = writeln!(
        err,
        "       rebuild with: cargo build --release --features linux-udev"
    );
    let _ = writeln!(
        err,
        "       (and ensure pkg-config + libudev-dev are installed on the host)"
    );
    ExitCode::from(1)
}

/// Common driver for every state-changing subcommand.
///
/// 1. Look up the library by serial; missing → exit 2.
/// 2. Build a [`StaticAllowlist`] from `--allow` / `--allow-derived`.
/// 3. Open the library (Linux-only path; non-Linux yields exit 1
///    with a friendly error).
/// 4. Run the caller's op closure; print `"ok: <summary>"` or the
///    error, with an EPERM/CAP_SYS_RAWIO hint when applicable.
///
/// Warnings from the discovery pass always print to stderr at the
/// end, matching the read-only subcommands' behaviour.
fn run_state_change<F>(
    report: &DiscoveryReport,
    serial: &str,
    allow: &[String],
    allow_derived: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
    op: F,
) -> ExitCode
where
    F: FnOnce(
        &mut remanence_library::LibraryHandle,
        &dyn remanence_library::AccessPolicy,
    ) -> Result<String, String>,
{
    let lib = match report.library(serial) {
        Some(l) => l,
        None => {
            let _ = writeln!(err, "error: no library with serial {serial:?} on this host");
            let _ = writeln!(err, "       run `rem libraries` to see what's available");
            print_warnings(report, err);
            return ExitCode::from(2);
        }
    };

    // The pre-discovery allowlist gate in `run()` has already
    // ensured the target library is on `--allow` — this function
    // is only reached for permitted libraries. `Library::open(policy)`
    // below is still the *real* safety check; this function trusts
    // the gate but doesn't itself police the allowlist.
    let mut policy = StaticAllowlist::new(allow.iter().cloned());
    for s in allow_derived {
        policy = policy.with_derived_allowed(s.clone());
    }

    open_and_run(lib, &policy, out, err, op, report)
}

#[cfg(target_os = "linux")]
fn open_and_run<F>(
    lib: &Library,
    policy: &dyn remanence_library::AccessPolicy,
    out: &mut dyn Write,
    err: &mut dyn Write,
    op: F,
    report: &DiscoveryReport,
) -> ExitCode
where
    F: FnOnce(
        &mut remanence_library::LibraryHandle,
        &dyn remanence_library::AccessPolicy,
    ) -> Result<String, String>,
{
    let mut handle = match open_library_handle(lib, policy) {
        Ok(h) => h,
        Err(e) => {
            let s = e.to_string();
            let _ = writeln!(err, "error: opening library: {s}");
            print_setcap_hint_if_error_text_matches(&s, err);
            print_warnings(report, err);
            return ExitCode::from(1);
        }
    };
    match op(&mut handle, policy) {
        Ok(summary) => {
            let _ = writeln!(out, "ok: {summary}");
            // The op succeeded, but it may still have left the cached
            // snapshot out of sync with physical state. Pick the
            // wording from the handle's reported cause rather than
            // assuming a flavor — `Some(_)` exactly when `is_dirty()`.
            if let Some(cause) = handle.dirty_cause() {
                let library_serial = handle.library().serial.clone();
                print_dirty_snapshot_recovery(&library_serial, DirtyReason::from(cause), err);
            }
            print_warnings(report, err);
            ExitCode::SUCCESS
        }
        Err(s) => {
            let _ = writeln!(err, "error: {s}");
            print_setcap_hint_if_error_text_matches(&s, err);
            // Per `docs/layer2b-design.md` §5.1, state-changing ops
            // can leave the snapshot dirty in three operationally
            // distinct ways — composed-op partial failure, IE-port
            // vendor divergence (success path), or a completion-
            // unknown transport failure on a single CDB. Read the
            // categorised cause from the handle so the recovery
            // hint matches what actually happened.
            if let Some(cause) = handle.dirty_cause() {
                let library_serial = handle.library().serial.clone();
                print_dirty_snapshot_recovery(&library_serial, DirtyReason::from(cause), err);
            }
            print_warnings(report, err);
            ExitCode::from(1)
        }
    }
}

/// Why the cached snapshot is dirty — controls the recovery-hint
/// wording. All flavors emit the same set of suggested commands,
/// they just describe the reason differently. One-to-one with
/// [`remanence_library::DirtyCause`]; the conversion is total via
/// [`From`].
#[derive(Debug, Clone, Copy)]
enum DirtyReason {
    /// A composed op (load/unload/export/import) failed partway
    /// through: an earlier phase changed library state, then a later
    /// phase failed.
    PartialFailure,
    /// An op succeeded but touched an IE port, where vendor behavior
    /// diverges (HPE parks visibly in the IE element, QuadStor
    /// vaults to a hidden pool). The snapshot's IE-full/empty bits
    /// can't be trusted without a rescan.
    VendorSemantics,
    /// A state-changing CDB failed with a completion-ambiguous
    /// transport error (driver timeout, bus reset, host adapter
    /// reset). The robot or drive may have actually executed the
    /// operation even though we didn't get a clean status back.
    /// Also covers `rescan`'s post-INIT errors and `refresh`'s
    /// shape-mismatch outcome — both leave the snapshot stale via
    /// a different mechanism.
    CompletionUnknown,
}

impl From<DirtyCause> for DirtyReason {
    fn from(cause: DirtyCause) -> Self {
        match cause {
            DirtyCause::PartialFailure => Self::PartialFailure,
            DirtyCause::VendorSemantics => Self::VendorSemantics,
            DirtyCause::CompletionUnknown => Self::CompletionUnknown,
        }
    }
}

/// Print an operator-facing recovery hint when
/// [`remanence_library::LibraryHandle::is_dirty`] is set. The cached
/// snapshot is out of sync with the physical library; the operator
/// should inspect or refresh before retrying. The suggested commands
/// include `--allow <serial>` so they actually pass the
/// `rem-debug` pre-discovery allowlist gate.
fn print_dirty_snapshot_recovery(library_serial: &str, reason: DirtyReason, err: &mut dyn Write) {
    let _ = writeln!(err);
    match reason {
        DirtyReason::PartialFailure => {
            let _ = writeln!(
                err,
                "warning: the operation partially succeeded — an earlier phase"
            );
            let _ = writeln!(
                err,
                "         changed library state before the later phase failed."
            );
            let _ = writeln!(err, "         Inspect or recover before retrying:");
        }
        DirtyReason::VendorSemantics => {
            let _ = writeln!(
                err,
                "warning: the operation touched an IE port. Post-move state"
            );
            let _ = writeln!(
                err,
                "         depends on vendor semantics (some libraries vault the"
            );
            let _ = writeln!(
                err,
                "         cartridge rather than park it in the IE element)."
            );
            let _ = writeln!(err, "         Confirm physical state before relying on it:");
        }
        DirtyReason::CompletionUnknown => {
            let _ = writeln!(
                err,
                "warning: the operation failed with a transport-level error;"
            );
            let _ = writeln!(
                err,
                "         the device may have actually executed it even though"
            );
            let _ = writeln!(
                err,
                "         the host didn't get a clean status back (driver"
            );
            let _ = writeln!(
                err,
                "         timeout, bus reset, etc.). Confirm physical state"
            );
            let _ = writeln!(err, "         before retrying:");
        }
    }
    let _ = writeln!(
        err,
        "             rem library {library_serial} --slots                    # see current state"
    );
    let _ = writeln!(
        err,
        "             rem-debug rescan  {library_serial} --allow {library_serial}   # force re-derive"
    );
}

#[cfg(not(target_os = "linux"))]
fn open_and_run<F>(
    _lib: &Library,
    _policy: &dyn remanence_library::AccessPolicy,
    _out: &mut dyn Write,
    err: &mut dyn Write,
    _op: F,
    report: &DiscoveryReport,
) -> ExitCode
where
    F: FnOnce(
        &mut remanence_library::LibraryHandle,
        &dyn remanence_library::AccessPolicy,
    ) -> Result<String, String>,
{
    let _ = writeln!(
        err,
        "error: state-changing rem subcommands require Linux \
         (Library::open is Linux-only in v0.1)"
    );
    print_warnings(report, err);
    ExitCode::from(1)
}

/// Surface a `setcap` hint when an error message looks like the
/// kernel SG layer's `EPERM` for non-whitelisted opcodes — the same
/// signature `INSTALL.md`'s "Host privileges" section warns about.
fn print_setcap_hint_if_error_text_matches(s: &str, err: &mut dyn Write) {
    if s.contains("EPERM") || s.contains("Operation not permitted") {
        let _ = writeln!(err);
        let _ = writeln!(
            err,
            "hint: this error looks like the kernel SCSI command filter refusing"
        );
        let _ = writeln!(
            err,
            "      a state-changing opcode without CAP_SYS_RAWIO. Try:"
        );
        let _ = writeln!(
            err,
            "          sudo setcap cap_sys_rawio+ep {}",
            rem_binary_path()
        );
        let _ = writeln!(
            err,
            "      …against the rem binary. See INSTALL.md \"Host privileges\"."
        );
    }
}

/// Return the path of the currently-running `rem` binary in a form
/// suitable for `setcap`. Falls back to a placeholder if
/// [`std::env::current_exe`] fails (shouldn't happen in normal
/// flow). `$0` in an interactive shell would resolve to the *shell*,
/// not to the binary being run, so the hint must compute the path
/// at runtime.
fn rem_binary_path() -> String {
    match std::env::current_exe() {
        Ok(p) => p.display().to_string(),
        Err(_) => "/path/to/rem".to_string(),
    }
}

// ===================================================================
//  Output formatters
// ===================================================================

fn print_probe(probe: &ProbeResult, out: &mut dyn Write) {
    let _ = writeln!(out, "format: {}", probe.format_id);
    let _ = writeln!(
        out,
        "confidence: {}",
        probe_confidence_text(probe.confidence)
    );
    let _ = writeln!(
        out,
        "source: {}",
        source_requirement_text(probe.source_requirement)
    );
}

fn print_archive_scan(report: &remanence_stream::ArchiveScanReport, out: &mut dyn Write) {
    let _ = writeln!(out, "entries: {}", report.scan.entries);
    let _ = writeln!(out, "damage-events: {}", report.scan.damage_events);
    let _ = writeln!(out, "archive-gaps: {}", report.scan.archive_gaps);
    for entry in &report.entries {
        let size = entry
            .size_bytes
            .map(|size| size.to_string())
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}",
            entry.file_id.as_str(),
            entry_kind_text(entry.kind),
            size,
            entry.path
        );
    }
    for damage in &report.damages {
        let _ = writeln!(
            out,
            "damage\t{}\t{}..{}\t{}",
            damage.file_id.as_str(),
            damage.start,
            damage.end,
            damage_status_text(damage.status)
        );
    }
    for gap in &report.archive_gaps {
        let _ = writeln!(
            out,
            "archive-gap\t{}..{}\t{}",
            gap.source_start,
            gap.source_end,
            archive_gap_cause_text(gap.cause)
        );
    }
}

fn print_archive_restore(
    report: &remanence_stream::ArchiveFilesystemRestoreReport,
    out: &mut dyn Write,
) {
    let _ = writeln!(out, "entries: {}", report.stream.entries);
    let _ = writeln!(out, "files-written: {}", report.files_written);
    let _ = writeln!(out, "directories-seen: {}", report.directories_seen);
    let _ = writeln!(out, "bytes-written: {}", report.bytes_written);
    let _ = writeln!(out, "damage-events: {}", report.stream.damage_events);
    let _ = writeln!(out, "archive-gaps: {}", report.stream.archive_gaps);
}

fn print_archive_recovery(report: &remanence_stream::ArchiveRecoveryReport, out: &mut dyn Write) {
    let complete = report
        .files
        .iter()
        .filter(|file| file.status == remanence_stream::RecoveryStatus::Complete)
        .count();
    let partial = report
        .files
        .iter()
        .filter(|file| file.status == remanence_stream::RecoveryStatus::Partial)
        .count();
    let missing = report
        .files
        .iter()
        .filter(|file| file.status == remanence_stream::RecoveryStatus::Missing)
        .count();
    let skipped = report
        .files
        .iter()
        .filter(|file| file.status == remanence_stream::RecoveryStatus::Skipped)
        .count();
    let _ = writeln!(out, "entries: {}", report.stream.entries);
    let _ = writeln!(out, "files-seen: {}", report.files_seen);
    let _ = writeln!(out, "files-written: {}", report.files_written);
    let _ = writeln!(out, "bytes-written: {}", report.bytes_written);
    let _ = writeln!(out, "damage-events: {}", report.stream.damage_events);
    let _ = writeln!(out, "archive-gaps: {}", report.stream.archive_gaps);
    let _ = writeln!(out, "manifest: {}", report.manifest_path.display());
    let _ = writeln!(
        out,
        "statuses: complete={complete} partial={partial} missing={missing} skipped={skipped}"
    );
    for file in &report.files {
        let clean = range_bytes(&file.recovered_ranges);
        let suspect = range_bytes(&file.suspect_ranges);
        let declared = file
            .declared_size
            .map(|size| size.to_string())
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(
            out,
            "recovery\t{}\t{}\t{}\tclean={clean}\tsuspect={suspect}\t{}\t{}",
            file.file_id,
            recovery_status_text(file.status),
            declared,
            file.output_path,
            file.path
        );
    }
}

fn print_damage_list(damages: &[DamageRange], err: &mut dyn Write) {
    if damages.is_empty() {
        return;
    }
    let _ = writeln!(err);
    let _ = writeln!(err, "damage events ({}):", damages.len());
    for damage in damages {
        let _ = writeln!(
            err,
            "  - {} {}..{} {}",
            damage.file_id.as_str(),
            damage.start,
            damage.end,
            damage_status_text(damage.status)
        );
    }
}

fn probe_confidence_text(confidence: ProbeConfidence) -> &'static str {
    match confidence {
        ProbeConfidence::NoMatch => "no-match",
        ProbeConfidence::Possible => "possible",
        ProbeConfidence::Probable => "probable",
        ProbeConfidence::Certain => "certain",
    }
}

fn source_requirement_text(source: SourceRequirement) -> &'static str {
    match source {
        SourceRequirement::ObjectBlocks => "object-blocks",
        SourceRequirement::PhysicalTapeRecords => "physical-tape-records",
        SourceRequirement::ByteStreamDump => "byte-stream-dump",
        SourceRequirement::ObjectBytes => "object-bytes",
    }
}

fn entry_kind_text(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::RegularFile => "regular",
        EntryKind::Directory => "directory",
        EntryKind::Symlink => "symlink",
        EntryKind::Hardlink => "hardlink",
        EntryKind::Special => "special",
    }
}

fn print_damage_list_from_recovery(
    files: &[remanence_stream::RecoveryFileRecord],
    err: &mut dyn Write,
) {
    let count: usize = files.iter().map(|file| file.damage_ranges.len()).sum();
    if count == 0 {
        return;
    }
    let _ = writeln!(err);
    let _ = writeln!(err, "damage events ({count}):");
    for file in files {
        for damage in &file.damage_ranges {
            let _ = writeln!(
                err,
                "  {} {}..{} {}",
                file.file_id, damage.start, damage.end, damage.status
            );
        }
    }
}

fn print_archive_gap_list(gaps: &[ArchiveGapRange], err: &mut dyn Write) {
    if gaps.is_empty() {
        return;
    }
    let _ = writeln!(err);
    let _ = writeln!(err, "archive gaps ({}):", gaps.len());
    for gap in gaps {
        let _ = writeln!(
            err,
            "  {}..{} {}",
            gap.source_start,
            gap.source_end,
            archive_gap_cause_text(gap.cause)
        );
    }
}

fn print_recovery_archive_gap_list(
    gaps: &[remanence_stream::RecoveryArchiveGapRecord],
    err: &mut dyn Write,
) {
    if gaps.is_empty() {
        return;
    }
    let _ = writeln!(err);
    let _ = writeln!(err, "archive gaps ({}):", gaps.len());
    for gap in gaps {
        let _ = writeln!(
            err,
            "  {}..{} {}",
            gap.source_start, gap.source_end, gap.cause
        );
    }
}

fn range_bytes(ranges: &[remanence_stream::RecoveryByteRange]) -> u64 {
    ranges
        .iter()
        .map(|range| range.end.saturating_sub(range.start))
        .sum()
}

fn recovery_status_text(status: remanence_stream::RecoveryStatus) -> &'static str {
    match status {
        remanence_stream::RecoveryStatus::Complete => "complete",
        remanence_stream::RecoveryStatus::Partial => "partial",
        remanence_stream::RecoveryStatus::Missing => "missing",
        remanence_stream::RecoveryStatus::Skipped => "skipped",
    }
}

fn damage_status_text(status: DamageStatus) -> &'static str {
    match status {
        DamageStatus::ChecksumFailed => "checksum-failed",
        DamageStatus::ReadError => "read-error",
        DamageStatus::Missing => "missing",
        DamageStatus::Unsupported => "unsupported",
    }
}

fn archive_gap_cause_text(cause: ArchiveGapCause) -> &'static str {
    match cause {
        ArchiveGapCause::UnrecognizedData => "unrecognized-data",
        ArchiveGapCause::ReadError => "read-error",
        ArchiveGapCause::Missing => "missing",
        ArchiveGapCause::Resync => "resync",
        ArchiveGapCause::Unsupported => "unsupported",
    }
}

fn print_libraries(report: &DiscoveryReport, out: &mut dyn Write) {
    if report.libraries.is_empty() {
        // discover() would have errored if there were truly no
        // libraries; reaching here means a degenerate state.
        let _ = writeln!(out, "(no libraries)");
        return;
    }
    for lib in &report.libraries {
        let inq = &lib.changer_inquiry;
        let vendor = inq.vendor_str().trim();
        let product = inq.product_str().trim();
        let n_loaded = lib.slots.iter().filter(|s| s.full).count();
        let _ = writeln!(out,
            "{serial}  {vendor} {product}  {sg}  ({drives} drives, {slots} slots [{loaded} loaded], {ie} IE)",
            serial = lib.serial,
            sg     = lib.changer_sg.display(),
            drives = lib.drive_bays.len(),
            slots  = lib.slots.len(),
            loaded = n_loaded,
            ie     = lib.ie_ports.len(),
        );
    }
}

fn print_libraries_json(report: &DiscoveryReport, out: &mut dyn Write) {
    let libraries = report
        .libraries
        .iter()
        .map(|lib| {
            let inq = &lib.changer_inquiry;
            json!({
                "serial": lib.serial.as_str(),
                "changer_sg": lib.changer_sg.display().to_string(),
                "changer_sysfs": lib.changer_sysfs.display().to_string(),
                "vendor": inq.vendor_str().trim(),
                "product": inq.product_str().trim(),
                "revision": inq.revision_str().trim(),
                "drive_count": lib.drive_bays.len(),
                "slot_count": lib.slots.len(),
                "loaded_slot_count": lib.slots.iter().filter(|slot| slot.full).count(),
                "ie_port_count": lib.ie_ports.len(),
            })
        })
        .collect::<Vec<_>>();
    let _ = writeln!(out, "{}", json!({ "libraries": libraries }));
}

fn print_library_json(lib: &Library, include_slots: bool, out: &mut dyn Write) {
    let inq = &lib.changer_inquiry;
    let drives = lib
        .drive_bays
        .iter()
        .map(|bay| {
            json!({
                "element_address": format!("0x{:04x}", bay.element_address),
                "element_address_raw": bay.element_address,
                "accessible": bay.accessible,
                "exception": element_exception_json(bay.exception),
                "loaded": bay.loaded,
                "loaded_tape": bay.loaded_tape.as_deref(),
                "source_slot": bay.source_slot.map(|slot| format!("0x{slot:04x}")),
                "source_slot_raw": bay.source_slot,
                "installed": bay.installed.as_ref().map(|drive| json!({
                    "serial": drive.serial.as_str(),
                    "identity_source": format!("{:?}", drive.identity_source),
                    "vendor": drive.vendor.as_deref(),
                    "product": drive.product.as_deref(),
                    "revision": drive.revision.as_deref(),
                    "sg_path": drive.sg_path.as_ref().map(|path| path.display().to_string()),
                    "sysfs_path": drive.sysfs_path.as_ref().map(|path| path.display().to_string()),
                })),
            })
        })
        .collect::<Vec<_>>();
    let slots = include_slots.then(|| {
        lib.slots
            .iter()
            .map(|slot| {
                json!({
                    "element_address": format!("0x{:04x}", slot.element_address),
                    "element_address_raw": slot.element_address,
                    "accessible": slot.accessible,
                    "exception": element_exception_json(slot.exception),
                    "full": slot.full,
                    "cartridge": slot.cartridge.as_deref(),
                    "cleaning": slot.cartridge.as_deref().is_some_and(|tag| tag.starts_with("CLN")),
                })
            })
            .collect::<Vec<_>>()
    });
    let ie_ports = lib
        .ie_ports
        .iter()
        .map(|ie| {
            json!({
                "element_address": format!("0x{:04x}", ie.element_address),
                "element_address_raw": ie.element_address,
                "accessible": ie.accessible,
                "exception": element_exception_json(ie.exception),
                "full": ie.full,
                "cartridge": ie.cartridge.as_deref(),
                "import_enabled": ie.import_enabled,
                "export_enabled": ie.export_enabled,
            })
        })
        .collect::<Vec<_>>();
    let payload = json!({
        "serial": lib.serial.as_str(),
        "changer_sg": lib.changer_sg.display().to_string(),
        "changer_sysfs": lib.changer_sysfs.display().to_string(),
        "vendor": inq.vendor_str().trim(),
        "product": inq.product_str().trim(),
        "revision": inq.revision_str().trim(),
        "chassis": lib.chassis_designator.as_ref().map(|id| id.as_hex()),
        "drive_count": lib.drive_bays.len(),
        "slot_count": lib.slots.len(),
        "loaded_slot_count": lib.slots.iter().filter(|slot| slot.full).count(),
        "ie_port_count": lib.ie_ports.len(),
        "drives": drives,
        "slots": slots,
        "ie_ports": ie_ports,
    });
    let _ = serde_json::to_writer_pretty(&mut *out, &payload);
    let _ = writeln!(out);
}

fn element_exception_json(exception: Option<ElementException>) -> Option<Value> {
    exception.map(|exception| {
        json!({
            "asc": format!("0x{:02x}", exception.asc),
            "asc_raw": exception.asc,
            "ascq": format!("0x{:02x}", exception.ascq),
            "ascq_raw": exception.ascq,
        })
    })
}

fn element_exception_suffix(exception: Option<ElementException>) -> String {
    exception
        .map(|exception| {
            format!(
                "   exception ASC/ASCQ=0x{:02x}/0x{:02x}",
                exception.asc, exception.ascq
            )
        })
        .unwrap_or_default()
}

fn print_library(lib: &Library, report: &DiscoveryReport, out: &mut dyn Write) {
    let inq = &lib.changer_inquiry;
    let vendor = inq.vendor_str().trim();
    let product = inq.product_str().trim();
    let revision = inq.revision_str().trim();

    let _ = writeln!(out, "Library {}", lib.serial);
    let _ = writeln!(
        out,
        "  Changer:  {vendor} {product} {revision}  {sg}  (sysfs {sysfs})",
        sg = lib.changer_sg.display(),
        sysfs = lib.changer_sysfs.display(),
    );
    match &lib.chassis_designator {
        Some(d) if d.as_naa().is_some() => {
            let naa = d.as_naa().unwrap();
            // If another library shares the chassis NAA, mention it.
            let mut shared: Vec<&str> = report
                .libraries
                .iter()
                .filter(|other| other.serial != lib.serial)
                .filter(|other| {
                    other.chassis_designator.as_ref().and_then(|c| c.as_naa()) == Some(naa)
                })
                .map(|other| other.serial.as_str())
                .collect();
            shared.sort();
            if shared.is_empty() {
                let _ = writeln!(out, "  Chassis:  0x{naa:016x}");
            } else {
                let _ = writeln!(
                    out,
                    "  Chassis:  0x{naa:016x}   (also shared with {})",
                    shared.join(", ")
                );
            }
        }
        Some(d) => {
            let _ = writeln!(out, "  Chassis:  {}", d.as_hex());
        }
        None => {}
    }

    let _ = writeln!(out, "  Drives:");
    if lib.drive_bays.is_empty() {
        let _ = writeln!(out, "    (none)");
    } else {
        for bay in &lib.drive_bays {
            match &bay.installed {
                Some(installed) => {
                    let dv = installed.vendor.as_deref().unwrap_or("?");
                    let dp = installed.product.as_deref().unwrap_or("?");
                    let dr = installed
                        .revision
                        .as_deref()
                        .map(|s| format!("({s})"))
                        .unwrap_or_default();
                    let dsg = installed
                        .sg_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(no /dev/sgN)".to_string());
                    let exception = element_exception_suffix(bay.exception);
                    let _ = writeln!(
                        out,
                        "    [0x{addr:04x}] {dv} {dp} {dr}  {dsg}  serial {serial}{exception}",
                        addr = bay.element_address,
                        serial = installed.serial,
                    );
                }
                None => {
                    let exception = element_exception_suffix(bay.exception);
                    let _ = writeln!(
                        out,
                        "    [0x{addr:04x}] (no identity — see warnings){exception}",
                        addr = bay.element_address,
                    );
                }
            }
        }
    }

    let n_loaded = lib.slots.iter().filter(|s| s.full).count();
    let n_empty = lib.slots.len() - n_loaded;
    let _ = writeln!(
        out,
        "  Slots:    {} ({} loaded, {} empty)",
        lib.slots.len(),
        n_loaded,
        n_empty
    );

    if lib.ie_ports.is_empty() {
        let _ = writeln!(out, "  IE:       (none configured)");
    } else {
        let _ = writeln!(out, "  IE:");
        for ie in &lib.ie_ports {
            print_ie(ie, out);
        }
    }
}

fn print_slots(lib: &Library, out: &mut dyn Write) {
    let _ = writeln!(out);
    let _ = writeln!(out, "Slots:");
    for slot in &lib.slots {
        print_slot(slot, out);
    }
}

fn print_slot(slot: &Slot, out: &mut dyn Write) {
    let exception = element_exception_suffix(slot.exception);
    if slot.full {
        let tag = slot.cartridge.as_deref().unwrap_or("(no voltag)");
        let suffix = if tag.starts_with("CLN") {
            "   (cleaning)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "  [0x{:04x}] full   {tag}{suffix}{exception}",
            slot.element_address
        );
    } else {
        let _ = writeln!(out, "  [0x{:04x}] empty{exception}", slot.element_address);
    }
}

fn print_ie(ie: &IePort, out: &mut dyn Write) {
    let exception = element_exception_suffix(ie.exception);
    let state = if ie.full {
        format!(
            "full   {}",
            ie.cartridge.as_deref().unwrap_or("(no voltag)")
        )
    } else {
        "empty".to_string()
    };
    let in_ = if ie.import_enabled { "in" } else { "—" };
    let out_ = if ie.export_enabled { "out" } else { "—" };
    let _ = writeln!(
        out,
        "    [0x{:04x}] {state}   (import:{in_} export:{out_}){exception}",
        ie.element_address
    );
}

fn print_warnings(report: &DiscoveryReport, err: &mut dyn Write) {
    print_warning_list(&report.warnings, err);
}

/// Print a list of [`DiscoveryWarning`]s to stderr with the standard
/// `warnings (N):` header. Used both from the success path (via
/// [`print_warnings`]) and from the fatal-error path in `main`, where
/// the warnings come out of a `DiscoveryError::NoLibraries { warnings }`.
fn print_warning_list(warnings: &[DiscoveryWarning], err: &mut dyn Write) {
    if warnings.is_empty() {
        return;
    }
    let _ = writeln!(err);
    let _ = writeln!(err, "warnings ({}):", warnings.len());
    for w in warnings {
        let _ = writeln!(err, "  - {}", format_warning(w));
    }
}

/// If every SCSI failure in `warnings` looks like a kernel-filter
/// `EPERM` (the missing-CAP_SYS_RAWIO signature), nudge the operator
/// toward `setcap` rather than making them read INSTALL.md or strace
/// `rem` to find out why.
fn print_setcap_hint_if_needed(warnings: &[DiscoveryWarning], err: &mut dyn Write) {
    let scsi_failures: Vec<&DiscoveryWarning> = warnings
        .iter()
        .filter(|w| matches!(w, DiscoveryWarning::ScsiError { .. }))
        .collect();
    if scsi_failures.is_empty() {
        return;
    }
    let all_eperm = scsi_failures.iter().all(|w| match w {
        // `ScsiError::TransportError` from sg_io renders as a message
        // containing "EPERM" or "Operation not permitted" depending on
        // platform; either spelling means the kernel filter rejected
        // the opcode. We accept both to be defensive across kernel
        // versions.
        DiscoveryWarning::ScsiError { summary, .. } => {
            summary.contains("EPERM") || summary.contains("Operation not permitted")
        }
        _ => false,
    });
    if !all_eperm {
        return;
    }
    let _ = writeln!(err);
    let _ = writeln!(
        err,
        "hint: every SCSI probe returned EPERM. This is the kernel SCSI command"
    );
    let _ = writeln!(
        err,
        "      filter refusing READ ELEMENT STATUS without CAP_SYS_RAWIO. Try:"
    );
    let _ = writeln!(
        err,
        "          sudo setcap cap_sys_rawio+ep {}",
        rem_binary_path()
    );
    let _ = writeln!(
        err,
        "      …against the rem binary. See INSTALL.md \"Host privileges\" for"
    );
    let _ = writeln!(err, "      the matching production systemd-unit recipe.");
}

// (continued — format_warning below; tests at the bottom of file)

fn format_warning(w: &DiscoveryWarning) -> String {
    match w {
        DiscoveryWarning::DeviceUnreachable { path, source } => {
            format!(
                "device unreachable: {} ({})",
                path.display(),
                source.message
            )
        }
        DiscoveryWarning::ScsiError {
            path,
            command,
            summary,
        } => {
            format!("scsi error on {}: {command}: {summary}", path.display())
        }
        DiscoveryWarning::DriveMappingDerived { library, method } => {
            format!("library {library}: drive identity derived via {method} — operations require explicit policy opt-in")
        }
        DiscoveryWarning::DriveMappingUnavailable { library } => {
            format!("library {library}: drive identity unavailable — affected bays have no /dev/sgN binding")
        }
        DiscoveryWarning::UnclaimedTape { sg_path, serial } => {
            format!(
                "tape device {} (serial {serial}) didn't match any library's bay",
                sg_path.display()
            )
        }
        DiscoveryWarning::DriveSerialAmbiguous {
            sg_path,
            serial,
            claimants,
        } => {
            format!(
                "tape device {} (serial {serial}) matched multiple drive bays: {}",
                sg_path.display(),
                claimants.join(", ")
            )
        }
        DiscoveryWarning::UnresolvedDrive {
            library,
            serial,
            element_address,
        } => {
            format!("library {library}: drive serial {serial} at element 0x{element_address:04x} has no /dev/sgN attached")
        }
        DiscoveryWarning::LayoutMismatch { library } => {
            format!("library {library}: RES layout differs from MODE SENSE 1Dh; RES wins")
        }
        DiscoveryWarning::MalformedVoltag {
            library,
            element_address,
        } => {
            format!("library {library}: malformed voltag at element 0x{element_address:04x}")
        }
    }
}

// =====================================================================
//  Tests — drive every subcommand against synthetic DiscoveryReports
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use remanence_library::{
        scsi, DriveBay, ElementLayout, FixtureTransport, IdentitySource, InstalledDrive, Library,
        RecordingLog, RecordingTransport, ScsiError, SgTransport, Slot, TimeoutClass,
    };
    use std::collections::VecDeque;
    use std::fs;
    use std::path::{Path, PathBuf};

    #[cfg(target_os = "linux")]
    fn chaos_env_guard() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex as StdMutex, OnceLock};

        static ENV_LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        ENV_LOCK
            .get_or_init(|| StdMutex::new(()))
            .lock()
            .expect("chaos env lock")
    }

    #[cfg(target_os = "linux")]
    struct ChaosEnvSnapshot {
        enabled: Option<std::ffi::OsString>,
        allow_real: Option<std::ffi::OsString>,
        state: Option<std::ffi::OsString>,
    }

    #[cfg(target_os = "linux")]
    impl ChaosEnvSnapshot {
        fn capture() -> Self {
            Self {
                enabled: std::env::var_os(remanence_chaos::ENV_CHAOS_ENABLED),
                allow_real: std::env::var_os(remanence_chaos::ENV_CHAOS_ALLOW_REAL),
                state: std::env::var_os(remanence_chaos::ENV_CHAOS_STATE),
            }
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ChaosEnvSnapshot {
        fn drop(&mut self) {
            restore_env(remanence_chaos::ENV_CHAOS_ENABLED, self.enabled.take());
            restore_env(
                remanence_chaos::ENV_CHAOS_ALLOW_REAL,
                self.allow_real.take(),
            );
            restore_env(remanence_chaos::ENV_CHAOS_STATE, self.state.take());
        }
    }

    #[cfg(target_os = "linux")]
    fn restore_env(name: &str, value: Option<std::ffi::OsString>) {
        if let Some(value) = value {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }

    #[cfg(target_os = "linux")]
    fn clear_chaos_env() {
        std::env::remove_var(remanence_chaos::ENV_CHAOS_ENABLED);
        std::env::remove_var(remanence_chaos::ENV_CHAOS_ALLOW_REAL);
        std::env::remove_var(remanence_chaos::ENV_CHAOS_STATE);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn chaos_real_gate_requires_enabled_and_allow_real() {
        let _guard = chaos_env_guard();
        let _snapshot = ChaosEnvSnapshot::capture();
        clear_chaos_env();

        assert!(!remanence_chaos::chaos_real_enabled_from_env());
        std::env::set_var(remanence_chaos::ENV_CHAOS_ENABLED, "1");
        assert!(!remanence_chaos::chaos_real_enabled_from_env());
        std::env::remove_var(remanence_chaos::ENV_CHAOS_ENABLED);
        std::env::set_var(remanence_chaos::ENV_CHAOS_ALLOW_REAL, "1");
        assert!(!remanence_chaos::chaos_real_enabled_from_env());
        std::env::set_var(remanence_chaos::ENV_CHAOS_ENABLED, "yes");
        assert!(remanence_chaos::chaos_real_enabled_from_env());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disabled_chaos_factory_builds_without_state_or_device_access() {
        let _guard = chaos_env_guard();
        let _snapshot = ChaosEnvSnapshot::capture();
        clear_chaos_env();

        let _factory = cli_transport_factory(CliTransportAccess::ReadOnly)
            .expect("disabled chaos does not require REM_CHAOS_STATE");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn enabled_chaos_factory_requires_state_before_device_access() {
        let _guard = chaos_env_guard();
        let _snapshot = ChaosEnvSnapshot::capture();
        clear_chaos_env();
        std::env::set_var(remanence_chaos::ENV_CHAOS_ENABLED, "1");
        std::env::set_var(remanence_chaos::ENV_CHAOS_ALLOW_REAL, "1");

        match cli_transport_factory(CliTransportAccess::ReadOnly) {
            Ok(_) => panic!("enabled real-hardware chaos must require REM_CHAOS_STATE"),
            Err(error) => {
                assert_eq!(error.kind, "Other");
                assert!(error.message.contains(remanence_chaos::ENV_CHAOS_STATE));
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn chaos_device_context_uses_linux_backend_and_drive_alias() {
        let ctx = chaos_device_ctx(Path::new("/dev/sg0"));

        assert_eq!(ctx.backend.as_deref(), Some("linux"));
        assert_eq!(ctx.drive_id.as_deref(), Some("drive1"));
    }

    /// Build a minimal `Library` value from fixture INQUIRY bytes.
    /// Caller can mutate the returned struct to add drives, slots, etc.
    fn fake_library(serial: &str) -> Library {
        Library {
            serial: serial.to_string(),
            changer_sg: PathBuf::from("/dev/sg-mock"),
            changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
            changer_inquiry: scsi::Inquiry::parse(include_bytes!(
                "../../../fixtures/inquiry/changer-msl-g3.bin"
            ))
            .unwrap(),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 1,
                drive_count: 0,
                slot_start: 1000,
                slot_count: 0,
                ie_start: 0,
                ie_count: 0,
            },
            drive_bays: vec![],
            slots: vec![],
            ie_ports: vec![],
        }
    }

    /// Bring it through `Cli::parse_from`, run it, and return
    /// `(exit_code, stdout, stderr)` so each test can assert on every
    /// channel the CLI writes to.
    fn invoke(
        argv: &[&str],
        report: Result<DiscoveryReport, DiscoveryError>,
    ) -> (ExitCode, String, String) {
        let cli = Cli::parse_from(argv);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run(cli, move || report, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("stdout is utf8"),
            String::from_utf8(err).expect("stderr is utf8"),
        )
    }

    fn command_help(mut command: clap::Command) -> String {
        let mut out = Vec::<u8>::new();
        command.write_long_help(&mut out).unwrap();
        String::from_utf8(out).expect("help is utf8")
    }

    fn tape_init_config_with_pool(block_size_bytes: u64) -> remanence_state::RemConfig {
        let root = std::env::temp_dir().join("remanence-cli-tape-init-test");
        remanence_state::RemConfig {
            daemon: remanence_state::DaemonConfig {
                state_dir: root.clone(),
                spool_dir: None,
                spool_tmpfs_ram_budget: None,
                io_memory_ceiling: remanence_state::DEFAULT_IO_MEMORY_CEILING_BYTES,
                append_staging_mode: remanence_state::AppendStagingMode::Serial,
                append_ring_bytes: remanence_state::DEFAULT_APPEND_RING_BYTES,
                append_ring_high_pct: 90,
                append_ring_low_pct: 25,
                default_idle_timeout_seconds: 1800,
                drive_idle_unload_seconds: remanence_state::DEFAULT_DRIVE_IDLE_UNLOAD_SECONDS,
                read_only: false,
                socket_path: None,
                listen: None,
                tls: None,
            },
            libraries: Vec::new(),
            tape_pools: vec![remanence_state::TapePoolConfig {
                id: "camera.copy-a".to_string(),
                display_name: None,
                copy_class: None,
                content_class: None,
                selection_policy: remanence_state::PoolSelectionPolicyName::CompleteOrFill,
                watermark_low: 0.92,
                watermark_high: 0.97,
                block_size_bytes,
                min_object_size_bytes: 0,
            }],
            tape_pool_rules: Vec::new(),
            drives: remanence_state::DrivesConfig::default(),
            cleaning: remanence_state::CleaningConfig::default(),
            livestatus: remanence_state::LiveStatusConfig::default(),
            tape_io: remanence_state::TapeIoConfig::default(),
            journal: remanence_state::JournalConfig {
                dir: root.join("journals"),
                require_trusted_volume: false,
            },
            audit: remanence_state::AuditConfig {
                dir: root.join("audit"),
                fsync: true,
                clock_forward_tolerance_seconds: 300,
            },
            index: remanence_state::IndexConfig {
                sqlite_path: root.join("index/rem-state.sqlite"),
            },
            cache: remanence_state::CacheConfig {
                tape_catalog_dir: root.join("cache/tapes"),
            },
        }
    }

    fn vpd80_response(serial: &str) -> Vec<u8> {
        let bytes = serial.as_bytes();
        let mut v = vec![0x08u8, 0x80, 0x00, bytes.len() as u8];
        v.extend_from_slice(bytes);
        v
    }

    fn changer_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec()
    }

    fn lto9_inquiry_response() -> Vec<u8> {
        include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
    }

    fn readiness_fixed_sense(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
        let mut sense = vec![0u8; 32];
        sense[0] = 0x70;
        sense[2] = key & 0x0f;
        sense[7] = 24;
        sense[12] = asc;
        sense[13] = ascq;
        sense
    }

    fn readiness_check_condition(key: u8, asc: u8, ascq: u8) -> ScsiError {
        ScsiError::CheckCondition {
            sense: readiness_fixed_sense(key, asc, ascq),
            bytes_transferred: 0,
        }
    }

    #[cfg(target_os = "linux")]
    fn simulated_media_readiness_probe_schedule(
        ready_at: StdDuration,
        steady_poll: StdDuration,
    ) -> Vec<StdDuration> {
        let mut elapsed = StdDuration::ZERO;
        let mut schedule = Vec::new();
        loop {
            schedule.push(elapsed);
            if elapsed >= ready_at {
                break;
            }
            elapsed += media_conditioning_poll_interval(elapsed, steady_poll);
        }
        schedule
    }

    #[cfg(target_os = "linux")]
    struct TurSequenceTransport<T> {
        inner: T,
        tur_results: VecDeque<Result<(), ScsiError>>,
    }

    #[cfg(target_os = "linux")]
    impl<T: SgTransport> SgTransport for TurSequenceTransport<T> {
        fn execute_in(
            &mut self,
            cdb: &[u8],
            buf: &mut [u8],
        ) -> Result<remanence_library::transport::TransferOutcome, ScsiError> {
            self.inner.execute_in(cdb, buf)
        }

        fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
            if cdb.first().copied() == Some(0x00) {
                return self.tur_results.pop_front().unwrap_or(Ok(()));
            }
            self.inner.execute_none(cdb)
        }

        fn execute_out(
            &mut self,
            cdb: &[u8],
            buf: &[u8],
        ) -> Result<remanence_library::transport::TransferOutcome, ScsiError> {
            self.inner.execute_out(cdb, buf)
        }

        fn set_timeout_for(&mut self, class: TimeoutClass) {
            self.inner.set_timeout_for(class);
        }
    }

    #[cfg(target_os = "linux")]
    fn poll_lto9_tur_sequence(
        suffix: &str,
        tur_results: Vec<Result<(), ScsiError>>,
    ) -> (
        MediaReadinessPoll,
        Vec<(u64, bool, MediaReadiness)>,
        usize,
        bool,
    ) {
        let lib_serial = format!("LIB-UA-{suffix}");
        let drive_serial = format!("DRV-UA-{suffix}");
        let drive_path = PathBuf::from(format!("/dev/sg-drive-ua-{suffix}"));
        let (handle, mut drive, log) = open_lto9_test_drive_with_tur_sequence(
            &lib_serial,
            &drive_serial,
            &drive_path,
            tur_results,
        );
        let mut polls = Vec::new();
        let result = poll_drive_media_readiness(
            &mut drive,
            MediaFamily::Lto9OrLater,
            true,
            StdDuration::from_secs(1),
            StdDuration::ZERO,
            || None,
            |event| {
                if let MediaReadinessPollEvent::Poll(poll) = event {
                    polls.push((poll.attempts, poll.timed_out, poll.readiness.clone()));
                }
                Ok(())
            },
        )
        .expect("poll loop completes");
        let dirty = handle.is_dirty();
        let log = log.borrow();
        let tur_count = log
            .iter()
            .filter(|cdb| cdb.as_slice() == [0, 0, 0, 0, 0, 0])
            .count();
        (result, polls, tur_count, dirty)
    }

    #[cfg(target_os = "linux")]
    fn open_lto9_test_drive_with_tur_sequence(
        lib_serial: &str,
        drive_serial: &str,
        drive_path: &Path,
        tur_results: Vec<Result<(), ScsiError>>,
    ) -> (remanence_library::LibraryHandle, DriveHandle, RecordingLog) {
        let mut lib = fake_library(lib_serial);
        lib.drive_bays = vec![DriveBay {
            element_address: 0x0100,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: drive_serial.to_string(),
                identity_source: IdentitySource::DvcidInline,
                vendor: Some("HPE".to_string()),
                product: Some("Ultrium 9-SCSI".to_string()),
                revision: Some("R3G3".to_string()),
                sg_path: Some(drive_path.to_path_buf()),
                sysfs_path: None,
            }),
            loaded: true,
            loaded_tape: Some("AOX030L9".to_string()),
            source_slot: Some(0x03eb),
        }];
        let policy = StaticAllowlist::new([lib_serial]);
        let log = RecordingLog::new();
        let log_cl = log.clone();
        let mut changer_slot = Some(vec![changer_inquiry_response(), vpd80_response(lib_serial)]);
        let mut drive_slot = Some(vec![lto9_inquiry_response(), vpd80_response(drive_serial)]);
        let mut tur_results = Some(VecDeque::from(tur_results));
        let drive_path_cl = drive_path.to_path_buf();
        let factory: CliTransportFactory = Box::new(move |path| {
            if path == Path::new("/dev/sg-mock") {
                let inner = FixtureTransport::new().with_responses(
                    changer_slot.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "changer transport opened twice".to_string(),
                        raw_os_error: None,
                    })?,
                );
                Ok(
                    Box::new(RecordingTransport::with_log(inner, log_cl.clone()))
                        as Box<dyn SgTransport>,
                )
            } else if path == drive_path_cl.as_path() {
                let inner =
                    FixtureTransport::new().with_responses(drive_slot.take().ok_or_else(|| {
                        IoErrorKind {
                            kind: "Other",
                            message: "drive transport opened twice".to_string(),
                            raw_os_error: None,
                        }
                    })?);
                let tur = TurSequenceTransport {
                    inner,
                    tur_results: tur_results.take().ok_or_else(|| IoErrorKind {
                        kind: "Other",
                        message: "drive TUR sequence opened twice".to_string(),
                        raw_os_error: None,
                    })?,
                };
                Ok(Box::new(RecordingTransport::with_log(tur, log_cl.clone()))
                    as Box<dyn SgTransport>)
            } else {
                Err(IoErrorKind {
                    kind: "NotFound",
                    message: format!("unexpected transport path {path:?}"),
                    raw_os_error: None,
                })
            }
        });
        let mut handle = lib.open_with(&policy, factory).expect("library opens");
        let drive = handle.open_drive(0x0100, &policy).expect("drive opens");
        (handle, drive, log)
    }

    fn record_test_media_readiness_operation(
        index: &mut remanence_state::CatalogIndex,
        operation_id: Uuid,
        library_serial: &str,
        drive_element: u16,
        barcode: &str,
        state: &str,
        dirty_scope: Option<&str>,
    ) {
        index
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id,
                run_id: None,
                library_serial: library_serial.to_string(),
                changer_sg: Some("/dev/sg8".to_string()),
                drive_element,
                drive_sg: Some("/dev/sg7".to_string()),
                drive_serial: Some("DRV-UA-test".to_string()),
                barcode: Some(barcode.to_string()),
                source_slot: Some(0x03eb),
                media_generation: Some(9),
                phase: "readiness_poll".to_string(),
                state: state.to_string(),
                dirty_scope: dirty_scope.map(ToOwned::to_owned),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .expect("record media-readiness operation");
    }

    fn lto9_loaded_test_library(serial: &str, barcode: &str) -> Library {
        let mut lib = fake_library(serial);
        lib.drive_bays = vec![DriveBay {
            element_address: 0x0100,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: "DRV-UA-test".to_string(),
                identity_source: IdentitySource::DvcidInline,
                vendor: Some("HPE".to_string()),
                product: Some("Ultrium 9-SCSI".to_string()),
                revision: Some("R3G3".to_string()),
                sg_path: Some(PathBuf::from("/dev/sg7")),
                sysfs_path: None,
            }),
            loaded: true,
            loaded_tape: Some(barcode.to_string()),
            source_slot: Some(0x03eb),
        }];
        lib
    }

    #[test]
    fn rem_help_hides_direct_hardware_commands() {
        let help = command_help(Cli::command());

        assert!(help.contains("\n  libraries"));
        assert!(help.contains("\n  daemon"));
        assert!(help.contains("\n  op"));
        assert!(help.contains("\n  catalog"));
        assert!(help.contains("\n  archive"));
        assert!(
            !help.contains("--allow"),
            "rem help should not advertise hidden compatibility flags:\n{help}"
        );
        for command in [
            "move", "load", "unload", "export", "import", "rescan", "lock", "unlock", "dev",
        ] {
            assert!(
                !help.contains(&format!("\n  {command}")),
                "rem help unexpectedly exposes {command}:\n{help}"
            );
        }
    }

    #[test]
    fn archive_capabilities_are_machine_readable_without_discovery() {
        let cli = Cli::try_parse_from(["rem", "archive", "capabilities"]).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(
            cli,
            || panic!("archive capabilities must not perform hardware discovery"),
            &mut out,
            &mut err,
        );
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(err.is_empty());
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            value["capabilities"],
            json!([
                "rao-envelope",
                "wrap-suite-hpke-v1",
                "ranged-ciphertext-extract"
            ])
        );
    }

    #[test]
    fn archive_reseal_fully_reseals_and_verifies() {
        let plaintext = vec![0x5a; 1024];
        let digest: [u8; 32] = Sha256::digest(&plaintext).into();
        let source_primary =
            remanence_aead::RecipientPrivateKey::new([1; 16], "source-safe", [7; 32]).unwrap();
        let source_recovery =
            remanence_aead::RecipientPrivateKey::new([2; 16], "source-escrow", [8; 32]).unwrap();
        let source_recipients = vec![
            source_primary.public_key(0).unwrap(),
            source_recovery.public_key(1).unwrap(),
        ];
        let (source_object, _) = remanence_aead::seal_to_vec(
            &plaintext,
            &EnvelopeSealOptions {
                common: SealOptions {
                    chunk_size: 512,
                    object_id: "reseal-object".to_string(),
                    plaintext_size: plaintext.len() as u64,
                    plaintext_digest: digest,
                },
                recipients: source_recipients,
            },
        )
        .unwrap();

        let next_primary =
            remanence_aead::RecipientPrivateKey::new([3; 16], "next-safe", [9; 32]).unwrap();
        let next_recovery =
            remanence_aead::RecipientPrivateKey::new([4; 16], "next-escrow", [10; 32]).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let object = temp.path().join("object-encrypted.rao");
        let source_private_path = temp.path().join("source-safe.raop");
        let next_primary_path = temp.path().join("next-safe.raor");
        let next_recovery_path = temp.path().join("next-escrow.raor");
        let output = temp.path().join("object-resealed.rao");
        let staging = temp.path().join("plaintext-staging");
        fs::create_dir(&staging).unwrap();
        fs::write(&object, source_object).unwrap();
        fs::write(&source_private_path, source_primary.serialize()).unwrap();
        fs::write(
            &next_primary_path,
            next_primary.public_key(0).unwrap().serialize().unwrap(),
        )
        .unwrap();
        fs::write(
            &next_recovery_path,
            next_recovery.public_key(1).unwrap().serialize().unwrap(),
        )
        .unwrap();

        let args = ArchiveResealArgs {
            object,
            private_key: source_private_path,
            recipients: vec![next_primary_path, next_recovery_path],
            out: output.clone(),
            staging_dir: Some(staging.clone()),
        };
        let mut failure_out = Vec::new();
        let mut failure_err = Vec::new();
        let failure = run_archive_reseal_with(&args, &mut failure_out, &mut failure_err, |args| {
            reseal_archive_object_with_verifier(args, |path, expected| {
                let mut bytes = fs::read(path).map_err(|error| error.to_string())?;
                bytes[RAO_HEADER_LEN] ^= 0x80;
                fs::write(path, bytes).map_err(|error| error.to_string())?;
                let actual = sha256_file(path)?;
                if actual != expected {
                    return Err(
                        "staged encrypted object hash differs from the sealer report".to_string(),
                    );
                }
                Ok(actual)
            })
        });
        assert_ne!(failure, ExitCode::SUCCESS);
        assert!(!output.exists(), "failed verification published --out");
        assert_eq!(fs::read_dir(&staging).unwrap().count(), 0);

        let mut report_out = Vec::new();
        let mut report_err = Vec::new();
        assert_eq!(
            run_archive_reseal(&args, &mut report_out, &mut report_err),
            ExitCode::SUCCESS
        );
        assert!(report_err.is_empty());
        let report: Value = serde_json::from_slice(&report_out).unwrap();
        assert_eq!(report["input_format_version"], 2);
        assert_eq!(report["verified_after_write"], true);
        assert_eq!(report["object_id"], "reseal-object");
        assert_eq!(report["chunk_size"], 512);
        assert_eq!(report["plaintext_digest"], bytes_to_hex(&digest));
        assert_eq!(fs::read_dir(staging).unwrap().count(), 0);

        let resealed = fs::read(output).unwrap();
        let (opened, opened_report) =
            remanence_aead::open_to_vec(&resealed, &next_recovery).unwrap();
        assert_eq!(opened, plaintext);
        assert_eq!(opened_report.header.format_version, 2);
        assert_eq!(opened_report.header.object_id, "reseal-object");
        assert_eq!(opened_report.header.chunk_size, 512);
        assert_eq!(opened_report.metadata.plaintext_digest, digest);
    }

    #[test]
    fn rem_debug_help_lists_direct_hardware_commands() {
        let help = command_help(DebugCli::command());

        for command in [
            "move", "load", "unload", "export", "import", "rescan", "lock", "unlock", "dev",
            "catalog",
        ] {
            assert!(
                help.contains(&format!("\n  {command}")),
                "rem-debug help should expose {command}:\n{help}"
            );
        }
        for command in ["daemon", "op", "daemon-catalog"] {
            assert!(
                !help.contains(&format!("\n  {command}")),
                "rem-debug help should not expose daemon client command {command}:\n{help}"
            );
        }
    }

    #[test]
    fn rem_archive_probe_help_is_dump_only() {
        let mut command = Cli::command();
        let archive = command.find_subcommand_mut("archive").unwrap();
        let probe = archive.find_subcommand_mut("probe").unwrap();
        let help = command_help(probe.clone());

        assert!(help.contains("--dump <PATH>"), "{help}");
        for hidden_arg in ["--tape", "--bay", "--rewind"] {
            assert!(
                !help.contains(hidden_arg),
                "rem archive probe help unexpectedly exposes {hidden_arg}:\n{help}"
            );
        }
    }

    #[test]
    fn rem_archive_help_exposes_daemon_verify_and_hides_direct_mutations() {
        let mut command = Cli::command();
        let archive = command.find_subcommand_mut("archive").unwrap();
        let help = command_help(archive.clone());

        for command in ["write", "read"] {
            assert!(
                !help.contains(&format!("\n  {command}")),
                "rem archive help should not expose direct tape command {command}:\n{help}"
            );
        }
        for command in [
            "build",
            "inspect",
            "extract",
            "extract-stream",
            "covering-range",
            "probe",
            "scan",
            "restore",
            "recover",
            "verify",
            "list",
        ] {
            assert!(
                help.contains(&format!("\n  {command}")),
                "rem archive help should expose {command}:\n{help}"
            );
        }
    }

    #[test]
    fn top_level_help_and_mode_gates_agree_for_rem_and_rem_debug() {
        let rem_help = command_help(Cli::command());
        assert!(rem_help.contains("\n  audit"), "{rem_help}");
        assert!(rem_help.contains("\n  drive"), "{rem_help}");
        assert!(!rem_help.contains("\n  load"), "{rem_help}");

        let debug_help = command_help(DebugCli::command());
        assert!(debug_help.contains("\n  load"), "{debug_help}");
        assert!(!debug_help.contains("\n  audit"), "{debug_help}");
        assert!(!debug_help.contains("\n  drive"), "{debug_help}");

        let rem_audit: ParsedCli = Cli::parse_from([
            "rem",
            "audit",
            "query",
            "--since",
            "2026-07-18T00:00:00Z",
            "--until",
            "2026-07-19T00:00:00Z",
        ])
        .into();
        assert!(rem_debug_only_reason(&rem_audit.command).is_none());

        let debug_load: ParsedCli = DebugCli::parse_from([
            "rem-debug",
            "load",
            "LIB",
            "--slot",
            "0x0400",
            "--bay",
            "0x0100",
        ])
        .into();
        assert!(rem_only_reason(&debug_load.command).is_none());

        let hidden_rem_load: ParsedCli =
            Cli::parse_from(["rem", "load", "LIB", "--slot", "0x0400", "--bay", "0x0100"]).into();
        assert!(rem_debug_only_reason(&hidden_rem_load.command).is_some());

        let hidden_debug_audit: ParsedCli = DebugCli::parse_from([
            "rem-debug",
            "audit",
            "query",
            "--since",
            "2026-07-18T00:00:00Z",
            "--until",
            "2026-07-19T00:00:00Z",
        ])
        .into();
        assert!(rem_only_reason(&hidden_debug_audit.command).is_some());
    }

    #[test]
    fn rem_drive_load_parses_barcode_or_slot_source() {
        let barcode = Cli::parse_from([
            "rem",
            "drive",
            "load",
            "--library",
            "mainlib",
            "--barcode",
            "ACM003L9",
            "--bay",
            "0x0101",
        ]);
        assert!(matches!(
            barcode.command,
            RemCommand::Drive {
                command: DriveClientCommand::Load(DriveLoadArgs {
                    barcode: Some(ref barcode),
                    slot: None,
                    bay: 0x0101,
                    ..
                }),
                ..
            } if barcode == "ACM003L9"
        ));

        let slot = Cli::parse_from([
            "rem",
            "drive",
            "load",
            "--library",
            "mainlib",
            "--slot",
            "0x0400",
            "--bay",
            "0x0102",
        ]);
        assert!(matches!(
            slot.command,
            RemCommand::Drive {
                command: DriveClientCommand::Load(DriveLoadArgs {
                    barcode: None,
                    slot: Some(0x0400),
                    bay: 0x0102,
                    ..
                }),
                ..
            }
        ));

        let no_wait = Cli::parse_from([
            "rem",
            "drive",
            "load",
            "--library",
            "mainlib",
            "--slot",
            "0x0400",
            "--bay",
            "0x0102",
            "--no-wait",
        ]);
        assert!(matches!(
            no_wait.command,
            RemCommand::Drive {
                command: DriveClientCommand::Load(DriveLoadArgs { no_wait: true, .. }),
                ..
            }
        ));

        assert!(Cli::try_parse_from([
            "rem",
            "drive",
            "load",
            "--library",
            "mainlib",
            "--bay",
            "0x0102",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "rem",
            "drive",
            "load",
            "--library",
            "mainlib",
            "--barcode",
            "ACM003L9",
            "--slot",
            "0x0400",
            "--bay",
            "0x0102",
        ])
        .is_err());
    }

    #[test]
    fn rem_audit_query_and_drive_verify_parse_supported_surfaces() {
        let audit = Cli::parse_from([
            "rem",
            "audit",
            "query",
            "--since",
            "2026-07-18T00:00:00Z",
            "--until",
            "2026-07-19T00:00:00Z",
            "--filter",
            "event_kind=OperationFailed",
            "--json",
        ]);
        assert!(matches!(
            audit.command,
            RemCommand::Audit {
                json: true,
                command: AuditClientCommand::Query { ref filters, .. },
                ..
            } if filters == &["event_kind=OperationFailed"]
        ));

        let verify = Cli::parse_from([
            "rem",
            "archive",
            "verify",
            "--library",
            "mainlib",
            "--drive",
            "0x0102",
            "--locator",
            "{}",
            "--expected-sha256",
            "00",
        ]);
        let parsed: ParsedCli = verify.into();
        assert!(matches!(
            parsed.command,
            Command::ArchiveVerifyClient {
                drive: 0x0102,
                ref library,
                ..
            } if library == "mainlib"
        ));
    }

    #[test]
    fn rem_archive_ranged_ciphertext_commands_parse_required_geometry() {
        let query = Cli::parse_from([
            "rem",
            "archive",
            "covering-range",
            "--private-key",
            "/tmp/key",
            "--object-id",
            "stored-object",
            "--file-id",
            "member-1",
            "--range",
            "100:25",
        ]);
        let RemCommand::Archive {
            command: RemArchiveCommand::CoveringRange(args),
        } = query.command
        else {
            panic!("expected covering-range command");
        };
        assert_eq!(args.object_id, "stored-object");
        assert_eq!(args.file_id, "member-1");
        assert_eq!(args.private_key, Some(PathBuf::from("/tmp/key")));
        assert_eq!(
            args.range,
            ArchiveByteRange {
                start: 100,
                len: 25
            }
        );

        let ranged = Cli::parse_from([
            "rem",
            "archive",
            "extract-stream",
            "--private-key",
            "/tmp/key",
            "--range",
            "100:25",
            "--authenticated-prefix",
            "/tmp/prefix",
            "--stored-range-start",
            "4096",
        ]);
        let RemCommand::Archive {
            command: RemArchiveCommand::ExtractStream(args),
        } = ranged.command
        else {
            panic!("expected extract-stream command");
        };
        assert_eq!(args.stored_range_start, Some(4096));
        assert_eq!(args.private_key, Some(PathBuf::from("/tmp/key")));
        assert_eq!(
            args.range,
            Some(ArchiveByteRange {
                start: 100,
                len: 25
            })
        );

        assert!(Cli::try_parse_from([
            "rem",
            "archive",
            "extract-stream",
            "--private-key",
            "/tmp/key",
            "--range",
            "100:25",
            "--authenticated-prefix",
            "/tmp/prefix",
        ])
        .is_err());
    }

    #[test]
    fn rem_archive_build_parses_local_file_command() {
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "build",
            "--inputs",
            "/tmp/input-a",
            "/tmp/input-b",
            "--out",
            "/tmp/object.rao",
            "--chunk-size",
            "4KiB",
        ]);

        match cli.command {
            RemCommand::Archive {
                command: RemArchiveCommand::Build(args),
            } => {
                assert_eq!(
                    args.inputs,
                    vec![PathBuf::from("/tmp/input-a"), PathBuf::from("/tmp/input-b")]
                );
                assert_eq!(args.out, Some(PathBuf::from("/tmp/object.rao")));
                assert_eq!(args.chunk_size, 4096);
                assert!(args.rules.is_none());
                assert!(!args.scan_only);
                assert!(args.manifest_out.is_none());
                assert!(!args.no_index);
                assert!(args.recipients.is_empty());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rem_archive_inspect_parses_local_file_command() {
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "inspect",
            "--object",
            "/tmp/object.rao",
            "--chunk-size",
            "4KiB",
        ]);

        match cli.command {
            RemCommand::Archive {
                command: RemArchiveCommand::Inspect(args),
            } => {
                assert_eq!(args.object, PathBuf::from("/tmp/object.rao"));
                assert_eq!(args.chunk_size, 4096);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rem_archive_extract_parses_local_file_command() {
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "extract",
            "--object",
            "/tmp/object.rao",
            "--dest",
            "/tmp/restored",
            "--chunk-size",
            "4KiB",
            "--overwrite",
        ]);

        match cli.command {
            RemCommand::Archive {
                command: RemArchiveCommand::Extract(args),
            } => {
                assert_eq!(args.object, PathBuf::from("/tmp/object.rao"));
                assert_eq!(args.dest, PathBuf::from("/tmp/restored"));
                assert_eq!(args.chunk_size, 4096);
                assert!(args.overwrite);
                assert!(args.private_key.is_none());
                assert!(args.path.is_none());
                assert!(args.first_chunk_lba.is_none());
                assert!(args.file_size_bytes.is_none());
                assert!(args.range.is_none());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rem_archive_extract_parses_local_file_range_command() {
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "extract",
            "--object",
            "/tmp/object.rao",
            "--dest",
            "/tmp/restored",
            "--private-key",
            "/tmp/primary.raop",
            "--path",
            "nested/big.bin",
            "--first-chunk-lba",
            "17",
            "--file-size-bytes",
            "2KiB",
            "--range",
            "512:768",
        ]);

        match cli.command {
            RemCommand::Archive {
                command: RemArchiveCommand::Extract(args),
            } => {
                assert_eq!(args.private_key, Some(PathBuf::from("/tmp/primary.raop")));
                assert_eq!(args.path.as_deref(), Some("nested/big.bin"));
                assert_eq!(args.first_chunk_lba, Some(17));
                assert_eq!(args.file_size_bytes, Some(2048));
                assert_eq!(
                    args.range,
                    Some(ArchiveByteRange {
                        start: 512,
                        len: 768
                    })
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rem_archive_extract_stream_parses_pipe_command() {
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "extract-stream",
            "--private-key",
            "/tmp/primary.raop",
            "--range",
            "511:514",
        ]);

        match cli.command {
            RemCommand::Archive {
                command: RemArchiveCommand::ExtractStream(args),
            } => {
                assert_eq!(args.private_key, Some(PathBuf::from("/tmp/primary.raop")));
                assert_eq!(
                    args.range,
                    Some(ArchiveByteRange {
                        start: 511,
                        len: 514
                    })
                );
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rem_archive_write_parses_encrypted_tape_command() {
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "write",
            "--library",
            "LIBSERIAL",
            "--file",
            "/tmp/payload.bin",
            "--pool",
            "scenario-a",
            "--archive-path",
            "payload.bin",
            "--caller-object-id",
            "caller-1",
            "--recipient",
            "/tmp/primary.raor",
            "--recipient",
            "/tmp/recovery.raor",
            "--json",
            "--config",
            "/tmp/rem.toml",
        ]);

        match cli.command {
            RemCommand::Archive {
                command: RemArchiveCommand::Write(args),
            } => {
                assert_eq!(args.library, "LIBSERIAL");
                assert_eq!(args.file, PathBuf::from("/tmp/payload.bin"));
                assert_eq!(args.pool, "scenario-a");
                assert_eq!(args.archive_path, Some(PathBuf::from("payload.bin")));
                assert_eq!(args.caller_object_id.as_deref(), Some("caller-1"));
                assert_eq!(
                    args.recipients,
                    vec![
                        PathBuf::from("/tmp/primary.raor"),
                        PathBuf::from("/tmp/recovery.raor")
                    ]
                );
                assert!(args.json);
                assert_eq!(args.config, PathBuf::from("/tmp/rem.toml"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rem_debug_archive_probe_help_lists_tape_source() {
        let mut command = DebugCli::command();
        let archive = command.find_subcommand_mut("archive").unwrap();
        let probe = archive.find_subcommand_mut("probe").unwrap();
        let help = command_help(probe.clone());

        for direct_arg in ["--tape", "--bay", "--rewind"] {
            assert!(
                help.contains(direct_arg),
                "rem-debug archive probe help should expose {direct_arg}:\n{help}"
            );
        }
    }

    #[test]
    fn debug_cli_parses_archive_read() {
        let cli = DebugCli::parse_from([
            "rem-debug",
            "--allow",
            "LIB",
            "archive",
            "read",
            "--library",
            "LIB",
            "--locator",
            "{}",
            "--out",
            "/tmp/restored.bin",
            "--config",
            "/tmp/config.toml",
        ]);
        assert!(matches!(
            cli.command,
            Command::Archive { command } if matches!(*command, ArchiveCommand::Read(_))
        ));
    }

    #[test]
    fn debug_cli_parses_archive_export_object() {
        let cli = DebugCli::parse_from([
            "rem-debug",
            "--allow",
            "LIB",
            "archive",
            "export-object",
            "--library",
            "LIB",
            "--locator",
            "{}",
            "--out",
            "/tmp/object.rao",
            "--config",
            "/tmp/config.toml",
        ]);
        assert!(matches!(
            cli.command,
            Command::Archive { command } if matches!(*command, ArchiveCommand::ExportObject(_))
        ));
    }

    #[test]
    fn debug_cli_parses_archive_verify() {
        let cli = DebugCli::parse_from([
            "rem-debug",
            "--allow",
            "LIB",
            "archive",
            "verify",
            "--library",
            "LIB",
            "--locator",
            "{}",
            "--expected-sha256",
            "00",
            "--config",
            "/tmp/config.toml",
        ]);
        assert!(matches!(
            cli.command,
            Command::Archive { command } if matches!(*command, ArchiveCommand::Verify(_))
        ));
    }

    #[test]
    fn debug_cli_parses_archive_list() {
        let cli = DebugCli::parse_from([
            "rem-debug",
            "archive",
            "list",
            "--config",
            "/tmp/config.toml",
        ]);
        assert!(matches!(
            cli.command,
            Command::Archive { command } if matches!(*command, ArchiveCommand::List(_))
        ));
    }

    #[test]
    fn rem_daemon_client_commands_parse_endpoint_and_json_flags() {
        let cli = Cli::parse_from([
            "rem",
            "catalog",
            "--endpoint",
            "http://127.0.0.1:50051",
            "--json",
            "tapes",
            "--pool",
            "copy-a",
        ]);

        match cli.command {
            RemCommand::Catalog {
                endpoint,
                json,
                command: CatalogClientCommand::Tapes { pool },
            } => {
                assert_eq!(endpoint, "http://127.0.0.1:50051");
                assert!(json);
                assert_eq!(pool.as_deref(), Some("copy-a"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn rem_daemon_client_commands_default_to_installed_unix_socket() {
        let cli = Cli::parse_from(["rem", "daemon", "health"]);

        match cli.command {
            RemCommand::Daemon { endpoint, .. } => {
                assert_eq!(endpoint, DEFAULT_DAEMON_ENDPOINT);
                assert!(endpoint.starts_with("unix:"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn daemon_health_json_uses_cli_envelope() {
        let mut components = std::collections::HashMap::new();
        components.insert("sqlite_index".to_string(), "ok".to_string());
        let mut out = Vec::<u8>::new();

        print_health(
            pb::HealthResponse {
                status: pb::health_response::Status::Healthy as i32,
                components,
                detail: "sqlite quick_check=ok".to_string(),
            },
            true,
            &mut out,
        )
        .unwrap();

        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["schema"], "rem.daemon.health.v1");
        assert_eq!(value["kind"], "item");
        assert_eq!(value["data"]["status"], "healthy");
        assert_eq!(value["data"]["components"]["sqlite_index"], "ok");
        assert!(value["operation"].is_null());
    }

    #[test]
    fn daemon_error_json_matches_cli_design_shape() {
        let mut err = Vec::<u8>::new();
        print_json_error("daemon_client_error", "connect failed", &mut err).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&err).unwrap();
        assert_eq!(value["schema"], "rem.error.v1");
        assert_eq!(value["kind"], "error");
        assert_eq!(value["code"], "daemon_client_error");
        assert_eq!(value["message"], "connect failed");
        assert!(value["details"].is_object());
        assert!(value.get("data").is_none());
    }

    #[test]
    fn daemon_error_json_uses_grpc_status_code_when_available() {
        let mut err = Vec::<u8>::new();
        let code = finish_daemon_client_result(
            Err(status_error(tonic::Status::failed_precondition(
                "drive busy",
            ))),
            true,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        let value: serde_json::Value = serde_json::from_slice(&err).unwrap();
        assert_eq!(value["schema"], "rem.error.v1");
        assert_eq!(value["kind"], "error");
        assert_eq!(value["code"], "failed_precondition");
        assert_eq!(
            value["message"],
            "daemon returned failed_precondition: drive busy"
        );
    }

    fn sample_live_status_response() -> pb::GetLiveStatusResponse {
        pb::GetLiveStatusResponse {
            libraries: vec![pb::LibraryState {
                library: Some(pb::Library {
                    library_serial: "MAINLIB".to_string(),
                    vendor: "HPE".to_string(),
                    product: "MSL".to_string(),
                    product_revision: "6.40".to_string(),
                    library_uuid: Uuid::from_u128(1).as_bytes().to_vec(),
                }),
                drives: vec![pb::Drive {
                    element_address: 0x0100,
                    drive_serial: "DRV-01".to_string(),
                    host_device_path: "/dev/sg1".to_string(),
                    vendor: "HPE".to_string(),
                    product: "LTO".to_string(),
                    loaded_tape_uuid: Uuid::from_u128(2).as_bytes().to_vec(),
                    status: pb::drive::Status::DriveStatusCleaning as i32,
                    drive_uuid: Uuid::from_u128(3).as_bytes().to_vec(),
                    cleaning_due: "now".to_string(),
                    fenced: true,
                    lifetime_read_bytes: 1_048_576,
                    lifetime_write_bytes: 2_097_152,
                    counter_epoch: 42,
                    session_id: Uuid::from_u128(4).as_bytes().to_vec(),
                    active_alert_names: vec!["cleaning".to_string()],
                    tape_io_staging_ring_buffers: 4,
                    tape_io_effective_batch_blocks: 16,
                    tape_io_gap_p95_us: 250,
                    tape_io_cadence_us: 1_100,
                    tape_io_effective_feed_bytes_per_second: 300_000_000,
                    loaded_tape_barcode: "CLN001".to_string(),
                    mount_age_seconds: 83,
                    tape_io_window_feed_bytes_per_second: 304_000_000,
                    ..Default::default()
                }],
                slots: vec![pb::Slot {
                    element_address: 0x0200,
                    voltag: "CLN001".to_string(),
                    tape_uuid: Uuid::from_u128(2).as_bytes().to_vec(),
                }],
                import_export_ports: Vec::new(),
                last_inventory_at: Some(prost_types::Timestamp {
                    seconds: 1,
                    nanos: 0,
                }),
                managed: "rem".to_string(),
            }],
            operations: vec![pb::OperationRef {
                operation_id: Uuid::from_u128(5).as_bytes().to_vec(),
            }],
            alarms: vec![pb::Alarm {
                alarm_id: 1,
                condition_key: "kind:scope".to_string(),
                kind: "kind".to_string(),
                severity: "warning".to_string(),
                state: "open".to_string(),
                first_seen_utc: Some(prost_types::Timestamp {
                    seconds: 1,
                    nanos: 0,
                }),
                last_seen_utc: Some(prost_types::Timestamp {
                    seconds: 1,
                    nanos: 0,
                }),
                acked_by: String::new(),
                acked_at_utc: None,
                detail: String::new(),
            }],
            snapshot_at_utc: "2026-07-04T00:00:00Z".to_string(),
            daemon_epoch: 17,
            drive_assignments: Vec::new(),
        }
    }

    #[test]
    fn top_json_uses_cli_envelope() {
        let mut out = Vec::<u8>::new();
        print_live_status_json(&sample_live_status_response(), &mut out).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["schema"], "rem.top.v1");
        assert_eq!(value["kind"], "item");
        assert_eq!(value["data"]["daemon_epoch"], 17);
        assert_eq!(value["data"]["libraries"][0]["managed"], "rem");
        assert_eq!(
            value["data"]["libraries"][0]["drives"][0]["status"],
            "cleaning"
        );
        assert!(value["data"]["libraries"][0]["drives"][0]["tape_io"]
            .get("pipelined_submission")
            .is_none());
        assert_eq!(
            value["data"]["libraries"][0]["drives"][0]["tape_io"]["gap_p95_us"],
            250
        );
        assert_eq!(
            value["data"]["libraries"][0]["drives"][0]["loaded_tape_barcode"],
            "CLN001"
        );
        assert_eq!(
            value["data"]["libraries"][0]["drives"][0]["mount_age_seconds"],
            83
        );
        assert_eq!(
            value["data"]["libraries"][0]["drives"][0]["tape_io"]["window_feed_bytes_per_second"],
            304_000_000
        );
        assert_eq!(
            value["data"]["libraries"][0]["drives"][0]["tape_io"]
                ["session_average_feed_bytes_per_second"],
            300_000_000
        );
        assert!(value["operation"].is_null());
    }

    #[test]
    fn top_text_labels_window_and_session_rates_with_barcode_age() {
        let mut out = Vec::new();
        print_live_status_text(&sample_live_status_response(), &mut out).expect("print top text");
        let text = String::from_utf8(out).expect("top text is utf-8");

        assert!(text.contains("tape_barcode=CLN001"), "{text}");
        assert!(text.contains("mount_age_s=83"), "{text}");
        assert!(text.contains("feed_window_Bps=304000000"), "{text}");
        assert!(text.contains("feed_session_avg_Bps=300000000"), "{text}");
        assert!(!text.contains(" feed_Bps="), "{text}");
    }

    #[test]
    fn top_unreachable_banner_points_to_library_command() {
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run_top_command(
            "unix:/tmp/rem-top-missing.sock",
            true,
            true,
            &mut out,
            &mut err,
        );

        assert_eq!(code, ExitCode::from(1));
        assert!(
            out.is_empty(),
            "unexpected stdout: {}",
            String::from_utf8_lossy(&out)
        );
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("rem library"), "{stderr}");
    }

    #[test]
    fn drive_status_renderer_passes_unknown_proto_enum_ints_through() {
        assert_eq!(drive_status_name(1), "idle");
        assert_eq!(drive_status_name(5), "cleaning");
        assert_eq!(drive_status_name(99), "unknown(99)");
    }

    #[test]
    fn drive_mutation_selector_accepts_uuid_or_serial_and_refuses_ambiguous_serial() {
        let uuid = Uuid::new_v4();
        assert_eq!(
            drive_uuid_from_selector(&uuid.to_string(), None).unwrap(),
            uuid.as_bytes().to_vec()
        );

        let resolved = Uuid::new_v4().as_bytes().to_vec();
        assert_eq!(
            drive_uuid_from_selector(
                "DRV123",
                Some(pb::DriveCatalogEntry {
                    drive_uuid: resolved.clone(),
                    serial: "DRV123".to_string(),
                    actionable: true,
                    ..Default::default()
                }),
            )
            .unwrap(),
            resolved
        );

        let err = drive_uuid_from_selector(
            "DUPSER",
            Some(pb::DriveCatalogEntry {
                drive_uuid: Uuid::new_v4().as_bytes().to_vec(),
                serial: "DUPSER".to_string(),
                actionable: false,
                ..Default::default()
            }),
        )
        .expect_err("ambiguous serial must refuse");
        assert!(err.contains("ambiguous"), "{err}");
    }

    #[test]
    fn drive_alerts_uses_live_poll_drive_request() {
        let request = poll_drive_request("DRV123");

        assert_eq!(request.drive, "DRV123");
        assert!(!request.allow_derived_identity);
    }

    #[test]
    fn drive_commands_map_unimplemented_to_upgrade_message() {
        let error = drive_status_error(tonic::Status::unimplemented("unknown method PollDrive"));

        assert_eq!(error.code, "daemon_client_error");
        assert_eq!(
            error.message,
            "daemon predates drive stewardship; upgrade rem-daemon"
        );
    }

    #[test]
    fn tape_wait_ready_accepts_resume_operation_id() {
        let operation_id = Uuid::from_u128(0xfeed);
        let operation_id_text = operation_id.to_string();
        let cli = Cli::parse_from([
            "rem",
            "tape",
            "wait-ready",
            "--resume",
            operation_id_text.as_str(),
        ]);
        match cli.command {
            RemCommand::Tape { command } => match command {
                RemTapeCommand::WaitReady(args) => {
                    assert_eq!(args.resume, Some(operation_id));
                    assert!(args.barcode.is_none());
                    assert!(args.drive_element.is_none());
                }
                other => panic!("unexpected tape command: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn tape_wait_ready_defaults_to_media_conditioning_profile() {
        let cli = Cli::parse_from(["rem", "tape", "wait-ready", "--barcode", "AOX030L9"]);
        match cli.command {
            RemCommand::Tape { command } => match command {
                RemTapeCommand::WaitReady(args) => {
                    assert_eq!(args.timeout, StdDuration::from_secs(9_000));
                    assert_eq!(args.poll, StdDuration::from_secs(30));
                }
                other => panic!("unexpected tape command: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn tape_wait_ready_new_barcode_refuses_active_media_readiness_fence() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-wait-ready-admission")
            .tempdir()
            .expect("temp dir");
        let mut index =
            remanence_state::CatalogIndex::open(temp.path().join("state.sqlite")).expect("open");
        let operation_id = Uuid::from_u128(0x9001);
        record_test_media_readiness_operation(
            &mut index,
            operation_id,
            "LIB-A",
            0x0100,
            "AOX030L9",
            "media_initializing",
            Some("drive+tape"),
        );
        let library = lto9_loaded_test_library("LIB-A", "AOX030L9");
        let args = TapeWaitReadyArgs {
            resume: None,
            barcode: Some("AOX030L9".to_string()),
            drive_element: None,
            already_loaded: false,
            wait: true,
            timeout: StdDuration::from_secs(1),
            poll: StdDuration::ZERO,
            config: PathBuf::from("/tmp/rem-config.toml"),
            library: Some("LIB-A".to_string()),
            json: false,
        };

        let err = resolve_wait_ready_operation(&library, &args, &mut index)
            .expect_err("new wait-ready must refuse an active fence");

        assert!(err.contains("active media-readiness operation"), "{err}");
        assert!(err.contains(&operation_id.to_string()), "{err}");
        assert_eq!(
            index
                .list_active_media_readiness_operations(Some("LIB-A"))
                .expect("active fences")
                .len(),
            1,
            "refused wait-ready must not create a duplicate operation"
        );
    }

    #[test]
    fn library_command_accepts_json_slots_snapshot() {
        let cli = Cli::parse_from(["rem", "library", "LIB-A", "--json", "--slots"]);
        match cli.command {
            RemCommand::Library {
                serial,
                slots,
                json,
            } => {
                assert_eq!(serial, "LIB-A");
                assert!(slots);
                assert!(json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn tape_quarantine_release_requires_settled_inventory_and_ack() {
        let missing_ack = Cli::try_parse_from([
            "rem",
            "tape",
            "quarantine",
            "release",
            "mrq-123",
            "--after-settled-inventory",
            "--ack",
            "",
        ])
        .expect("clap accepts empty string; semantic validation catches it");
        match missing_ack.command {
            RemCommand::Tape { command } => match command {
                RemTapeCommand::Quarantine { command } => {
                    assert!(command.validate_before_discovery().is_err());
                }
                other => panic!("unexpected tape command: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }

        let valid = Cli::parse_from([
            "rem",
            "tape",
            "quarantine",
            "release",
            "mrq-123",
            "--after-settled-inventory",
            "--ack",
            "operator checked settled inventory",
            "--json",
        ]);
        match valid.command {
            RemCommand::Tape { command } => match command {
                RemTapeCommand::Quarantine {
                    command:
                        TapeQuarantineCommand::Release {
                            quarantine,
                            after_settled_inventory,
                            ack,
                            json,
                            ..
                        },
                } => {
                    assert_eq!(quarantine, "mrq-123");
                    assert!(after_settled_inventory);
                    assert_eq!(ack, "operator checked settled inventory");
                    assert!(json);
                }
                other => panic!("unexpected tape command: {other:?}"),
            },
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn media_readiness_unknown_states_get_quarantine_id() {
        let operation_id = Uuid::from_u128(0x123);
        let poll = MediaReadinessPoll {
            readiness: MediaReadiness::TransportUnknown {
                detail: "DID_TIME_OUT".to_string(),
            },
            attempts: 1,
            timed_out: false,
        };
        let transition = media_readiness_transition_input(operation_id, "readiness_poll", &poll);
        assert_eq!(transition.state, "transport_unknown");
        assert_eq!(
            transition.quarantine_id.as_deref(),
            Some(format!("mrq-{operation_id}").as_str())
        );
    }

    #[test]
    fn media_readiness_signal_transition_records_aborted_unknown_fence() {
        let operation_id = Uuid::from_u128(0x123);
        let transition =
            media_readiness_signal_transition(operation_id, "readiness_poll", "SIGINT");

        assert_eq!(transition.state, "aborted_unknown");
        assert_eq!(transition.dirty_scope.as_deref(), Some("drive+tape"));
        assert_eq!(transition.cancel_source.as_deref(), Some("signal"));
        assert_eq!(transition.signal.as_deref(), Some("SIGINT"));
        assert_eq!(transition.transport_class.as_deref(), Some("unknown"));
        assert_eq!(
            transition.quarantine_id.as_deref(),
            Some(format!("mrq-{operation_id}").as_str())
        );
        assert!(media_readiness_state_requires_release("aborted_unknown"));
    }

    #[test]
    fn media_readiness_signal_request_persists_to_catalog_index() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-readiness-signal")
            .tempdir()
            .expect("temp dir");
        let mut index = remanence_state::CatalogIndex::open(temp.path().join("state.sqlite"))
            .expect("open catalog");
        let operation_id = Uuid::from_u128(0x456);
        index
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id,
                run_id: None,
                library_serial: "LIB-A".to_string(),
                changer_sg: Some("/dev/sg8".to_string()),
                drive_element: 0x0001,
                drive_sg: Some("/dev/sg7".to_string()),
                drive_serial: Some("DRV1".to_string()),
                barcode: Some("AOX030L9".to_string()),
                source_slot: Some(0x03eb),
                media_generation: Some(9),
                phase: "readiness_poll".to_string(),
                state: "media_initializing".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .expect("record operation");

        let err = record_media_readiness_signal_if_requested(
            &mut index,
            operation_id,
            "readiness_poll",
            || Some("SIGTERM"),
        )
        .expect_err("signal should abort readiness flow");
        assert!(err.contains("SIGTERM"), "{err}");

        let record = index
            .media_readiness_operation(operation_id)
            .expect("lookup")
            .expect("operation exists");
        assert_eq!(record.state, "aborted_unknown");
        assert_eq!(record.dirty_scope.as_deref(), Some("drive+tape"));
        assert_eq!(record.cancel_source.as_deref(), Some("signal"));
        assert_eq!(record.signal.as_deref(), Some("SIGTERM"));
        assert_eq!(
            record.quarantine_id.as_deref(),
            Some(format!("mrq-{operation_id}").as_str())
        );
        assert_eq!(
            index
                .list_active_media_readiness_operations(Some("LIB-A"))
                .expect("active fences")
                .len(),
            1
        );
    }

    #[test]
    fn media_readiness_signal_failure_overrides_command_failure() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-readiness-signal-failure")
            .tempdir()
            .expect("temp dir");
        let mut index = remanence_state::CatalogIndex::open(temp.path().join("state.sqlite"))
            .expect("open catalog");
        let operation_id = Uuid::from_u128(0x457);
        index
            .record_media_readiness_operation(remanence_state::MediaReadinessOperationInput {
                operation_id,
                run_id: None,
                library_serial: "LIB-A".to_string(),
                changer_sg: Some("/dev/sg8".to_string()),
                drive_element: 0x0001,
                drive_sg: Some("/dev/sg7".to_string()),
                drive_serial: Some("DRV1".to_string()),
                barcode: Some("AOX030L9".to_string()),
                source_slot: Some(0x03eb),
                media_generation: Some(9),
                phase: "readiness_poll".to_string(),
                state: "media_initializing".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                deadline_at_utc: None,
                evidence_path: None,
            })
            .expect("record operation");

        let message = record_media_readiness_signal_or_command_failure(
            &mut index,
            operation_id,
            "rewind_before_bot_read",
            Some(0x01),
            "interrupted system call",
            Some("SIGINT"),
        )
        .expect("record signal")
        .expect("signal message");

        assert!(media_readiness_error_is_signal_abort(&message));
        let record = index
            .media_readiness_operation(operation_id)
            .expect("lookup")
            .expect("operation exists");
        assert_eq!(record.state, "aborted_unknown");
        assert_eq!(record.cancel_source.as_deref(), Some("signal"));
        assert_eq!(record.signal.as_deref(), Some("SIGINT"));
        assert_eq!(record.last_cdb_opcode, None);
    }

    #[test]
    fn tape_wait_ready_signal_poll_error_uses_signal_exit_code() {
        let failure = tape_wait_ready_failure_from_poll_error(
            "media readiness interrupted by SIGTERM; recorded aborted_unknown fence".to_string(),
        );
        assert_eq!(failure.exit_code, 130);

        let failure = tape_wait_ready_failure_from_poll_error(
            "record media readiness transition: sqlite busy".to_string(),
        );
        assert_eq!(failure.exit_code, 1);

        let failure = tape_wait_ready_failure_from_poll_error(
            "media_readiness_state=transport_unknown media_readiness_exit_code=40: immediate drive load 0x0100: SG_IO transport error".to_string(),
        );
        assert_eq!(failure.exit_code, 40);
    }

    #[test]
    fn media_readiness_admission_error_names_quarantine_and_operation() {
        let operation_id = Uuid::from_u128(0xabc);
        let error = media_readiness_admission_error(
            "tape init",
            &[remanence_state::MediaReadinessOperationRecord {
                operation_id: operation_id.to_string(),
                run_id: None,
                library_serial: "LIB-A".to_string(),
                changer_sg: None,
                drive_element: 0x0002,
                drive_sg: None,
                drive_serial: Some("DRV2".to_string()),
                barcode: Some("AOX032L9".to_string()),
                source_slot: Some(0x03ed),
                media_generation: Some(9),
                phase: "readiness_poll".to_string(),
                state: "media_initializing".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                started_at_utc: "2026-07-06T00:00:00Z".to_string(),
                updated_at_utc: "2026-07-06T00:01:00Z".to_string(),
                deadline_at_utc: None,
                last_cdb_opcode: Some(0),
                last_sense_raw: None,
                last_sense_key: Some(2),
                last_asc: Some(4),
                last_ascq: Some(1),
                last_host_status: None,
                last_driver_status: None,
                target_status: None,
                transport_class: None,
                cancel_source: None,
                signal: None,
                evidence_path: None,
                last_error_json: None,
                quarantine_id: Some("mrq-custom".to_string()),
            }],
        );

        assert!(error.contains("tape init is blocked"));
        assert!(error.contains("mrq-custom"));
        assert!(error.contains(operation_id.to_string().as_str()));
        assert!(error.contains("AOX032L9"));
        assert!(error.contains("media_initializing"));
    }

    #[test]
    fn media_readiness_command_failure_state_keeps_unknown_completion_fenced() {
        assert_eq!(
            media_readiness_command_failure_state(
                "SG_IO transport error: host_status=0x0003 completion unknown"
            ),
            "transport_unknown"
        );
        assert_eq!(
            media_readiness_command_failure_state("drive rejected MODE SENSE"),
            "terminal_error"
        );
    }

    #[test]
    fn media_readiness_state_name_distinguishes_lto9_initializing() {
        assert_eq!(
            media_readiness_state_name(
                &MediaReadiness::BecomingReady {
                    ascq: 0x01,
                    media_initializing: true,
                },
                false,
            ),
            "media_initializing"
        );
        assert_eq!(
            media_readiness_state_name(
                &MediaReadiness::BecomingReady {
                    ascq: 0x01,
                    media_initializing: false,
                },
                false,
            ),
            "becoming_ready"
        );
        assert_eq!(
            media_readiness_state_name(&MediaReadiness::Ready, true),
            "timeout_unknown"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn media_conditioning_poll_ramp_detects_short_settle_before_thirty_seconds() {
        let schedule = simulated_media_readiness_probe_schedule(
            StdDuration::from_secs(8),
            StdDuration::from_secs(30),
        );

        assert_eq!(
            schedule,
            vec![
                StdDuration::from_secs(0),
                StdDuration::from_secs(1),
                StdDuration::from_secs(2),
                StdDuration::from_secs(3),
                StdDuration::from_secs(4),
                StdDuration::from_secs(5),
                StdDuration::from_secs(7),
                StdDuration::from_secs(9),
            ]
        );
        assert!(
            schedule.last().copied().unwrap() <= StdDuration::from_secs(10),
            "ready-at-8s media must not wait for the 30s steady poll"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn media_conditioning_poll_ramp_uses_configured_steady_poll() {
        let schedule = simulated_media_readiness_probe_schedule(
            StdDuration::from_secs(130),
            StdDuration::from_secs(17),
        );
        let steady_tail = schedule
            .windows(2)
            .filter(|window| window[0] >= StdDuration::from_secs(60))
            .map(|window| window[1] - window[0])
            .collect::<Vec<_>>();

        assert!(!steady_tail.is_empty());
        assert!(
            steady_tail
                .iter()
                .all(|delta| *delta == StdDuration::from_secs(17)),
            "steady-state deltas must respect --poll: {schedule:?}"
        );
        assert_eq!(
            media_conditioning_poll_interval(
                StdDuration::from_secs(60),
                StdDuration::from_secs(17)
            ),
            StdDuration::from_secs(17)
        );
    }

    #[test]
    fn tape_wait_ready_json_includes_operator_guidance() {
        let operation_id = Uuid::from_u128(0x30);
        let result = TapeWaitReadyResult {
            operation_id,
            drive_element: 0x0001,
            barcode: Some("AOX030L9".to_string()),
            readiness: MediaReadiness::BecomingReady {
                ascq: 0x01,
                media_initializing: true,
            },
            attempts: 1,
            timed_out: false,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();

        print_tape_wait_ready_result("LIBMAIN", &result, true, &mut out, &mut err);

        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["schema"], "rem.tape.wait_ready.v1");
        assert_eq!(value["state"], "media_initializing");
        assert_eq!(value["exit_code"], 10);
        assert!(value["operator_action"]
            .as_str()
            .unwrap()
            .contains("do not move"));
        assert!(value["recommended_next_command"]
            .as_str()
            .unwrap()
            .contains(&operation_id.to_string()));
        assert!(err.is_empty());
    }

    #[test]
    fn tape_wait_ready_human_timeout_includes_operator_guidance_on_stderr() {
        let operation_id = Uuid::from_u128(0x31);
        let result = TapeWaitReadyResult {
            operation_id,
            drive_element: 0x0001,
            barcode: Some("AOX031L9".to_string()),
            readiness: MediaReadiness::BecomingReady {
                ascq: 0x01,
                media_initializing: true,
            },
            attempts: 2,
            timed_out: true,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();

        print_tape_wait_ready_result("LIBMAIN", &result, false, &mut out, &mut err);

        let stdout = String::from_utf8(out).unwrap();
        let stderr = String::from_utf8(err).unwrap();
        assert!(stdout.contains("timeout operation_id="));
        assert!(stderr.contains("operator_action: timeout_unknown"));
        assert!(stderr.contains("recommended_next_command: rem tape quarantine show"));
        assert!(stderr.contains(&operation_id.to_string()));
    }

    #[test]
    fn tape_wait_ready_failure_json_classifies_ownership_refusal() {
        let failure = TapeWaitReadyFailure {
            message: "barcode AOX030L9 is in slot 0x03eb; wait-ready does not move media"
                .to_string(),
            exit_code: 50,
        };
        let mut out = Vec::new();
        let mut err = Vec::new();

        print_tape_wait_ready_failure("LIBMAIN", &failure, true, &mut out, &mut err);

        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["state"], "ownership_refused");
        assert_eq!(value["exit_code"], 50);
        assert!(value["operator_action"]
            .as_str()
            .unwrap()
            .contains("selected library"));
        assert_eq!(
            value["recommended_next_command"],
            "rem library LIBMAIN --slots"
        );
        assert!(err.is_empty());
    }

    #[test]
    fn tape_init_conditional_load_only_for_explicit_load_readiness() {
        assert!(!readiness_requires_conditional_load(
            &MediaReadiness::BecomingReady {
                ascq: 0x01,
                media_initializing: true,
            },
            true,
        ));
        assert!(readiness_requires_conditional_load(
            &MediaReadiness::BecomingReady {
                ascq: 0x02,
                media_initializing: true,
            },
            false,
        ));
        assert!(readiness_requires_conditional_load(
            &MediaReadiness::NoMedium { ascq: 0x00 },
            true,
        ));
        assert!(!readiness_requires_conditional_load(
            &MediaReadiness::NoMedium { ascq: 0x00 },
            false,
        ));
    }

    #[test]
    fn tape_init_readiness_error_carries_stable_exit_metadata() {
        let poll = MediaReadinessPoll {
            readiness: MediaReadiness::BecomingReady {
                ascq: 0x01,
                media_initializing: true,
            },
            attempts: 1,
            timed_out: false,
        };

        let error = tape_init_readiness_error("AOX030L9", 0x0001, &poll);

        assert!(error.contains("media_readiness_state=media_initializing"));
        assert!(error.contains("media_readiness_exit_code=10"));
        assert_eq!(tape_init_failure_exit_code(&error), Some(10));
        assert_eq!(tape_init_failure_exit_code("ordinary init failure"), None);
    }

    #[test]
    fn tape_init_selected_library_refuses_barcode_only_seen_elsewhere() {
        let mut config = tape_init_config_with_pool(262_144);
        config.libraries = vec![
            remanence_state::LibraryConfig {
                serial: "LIB-REM".to_string(),
                allow_derived_drive_identity: false,
            },
            remanence_state::LibraryConfig {
                serial: "LIB-D2".to_string(),
                allow_derived_drive_identity: false,
            },
        ];

        let mut rem_lib = fake_library("LIB-REM");
        rem_lib.drive_bays = vec![DriveBay {
            element_address: 0x0001,
            accessible: true,
            exception: None,
            installed: Some(InstalledDrive {
                serial: "REM-DRV".to_string(),
                identity_source: IdentitySource::DvcidInline,
                vendor: Some("HPE".to_string()),
                product: Some("Ultrium 9-SCSI".to_string()),
                revision: Some("R3G3".to_string()),
                sg_path: Some(PathBuf::from("/dev/sg-rem-drive")),
                sysfs_path: None,
            }),
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }];
        let mut d2_lib = fake_library("LIB-D2");
        d2_lib.slots = vec![Slot {
            element_address: 0x03eb,
            accessible: true,
            exception: None,
            full: true,
            cartridge: Some("AOX030L9".to_string()),
        }];
        let report = DiscoveryReport {
            libraries: vec![rem_lib, d2_lib],
            warnings: Vec::new(),
        };
        let target = TapeInitTarget::Voltag("AOX030L9".to_string());

        let err = resolve_tape_init_candidates(&report, &config, Some("LIB-REM"), &target)
            .expect_err("selected library must not search foreign partitions");
        assert!(
            err.contains("was not found in configured discovered libraries"),
            "{err}"
        );

        let candidates = resolve_tape_init_candidates(&report, &config, Some("LIB-D2"), &target)
            .expect("barcode is resolvable only when that library is selected");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].library_serial, "LIB-D2");
        assert_eq!(candidates[0].location, TapeInitLocation::Slot);
        assert_eq!(candidates[0].element_address, 0x03eb);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn media_readiness_poll_terminalizes_repeated_unit_attention() {
        let (result, polls, tur_count, dirty) = poll_lto9_tur_sequence(
            "repeat-reset",
            vec![
                Err(readiness_check_condition(0x06, 0x29, 0x00)),
                Err(readiness_check_condition(0x06, 0x29, 0x00)),
                Ok(()),
            ],
        );

        assert_eq!(result.attempts, 2);
        assert_eq!(
            result.readiness,
            MediaReadiness::RepeatedUnitAttention {
                asc: 0x29,
                ascq: 0x00
            }
        );
        assert_eq!(result.readiness.design_exit_code(), 30);
        assert_eq!(
            media_readiness_durable_state(&result.readiness, false),
            "terminal_error"
        );
        assert_eq!(polls.len(), 2);
        assert_eq!(polls[0].0, 1);
        assert!(!polls[0].1);
        assert!(matches!(
            polls[0].2,
            MediaReadiness::UnitAttention {
                asc: 0x29,
                ascq: 0x00
            }
        ));
        assert_eq!(polls[1].0, 2);
        assert!(!polls[1].1);
        assert!(matches!(
            polls[1].2,
            MediaReadiness::RepeatedUnitAttention {
                asc: 0x29,
                ascq: 0x00
            }
        ));
        assert_eq!(tur_count, 2, "poll loop must stop at repeated identical UA");
        assert!(!dirty, "TUR CHECK CONDITION is classified, not dirty");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn media_readiness_poll_allows_reset_then_not_ready_to_ready_unit_attention() {
        let (result, polls, tur_count, dirty) = poll_lto9_tur_sequence(
            "reset-then-ready",
            vec![
                Err(readiness_check_condition(0x06, 0x29, 0x00)),
                Err(readiness_check_condition(0x02, 0x04, 0x01)),
                Err(readiness_check_condition(0x06, 0x28, 0x00)),
                Ok(()),
            ],
        );

        assert_eq!(result.attempts, 4);
        assert_eq!(result.readiness, MediaReadiness::Ready);
        assert_eq!(result.readiness.design_exit_code(), 0);
        assert_eq!(
            media_readiness_durable_state(&result.readiness, false),
            "ready"
        );
        assert_eq!(polls.len(), 4);
        assert!(matches!(
            polls[0].2,
            MediaReadiness::UnitAttention {
                asc: 0x29,
                ascq: 0x00
            }
        ));
        assert!(matches!(
            polls[1].2,
            MediaReadiness::BecomingReady {
                ascq: 0x01,
                media_initializing: true
            }
        ));
        assert!(matches!(
            polls[2].2,
            MediaReadiness::UnitAttention {
                asc: 0x28,
                ascq: 0x00
            }
        ));
        assert_eq!(polls[3].2, MediaReadiness::Ready);
        assert_eq!(tur_count, 4);
        assert!(!dirty, "TUR CHECK CONDITION is classified, not dirty");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn media_readiness_poll_already_loaded_no_medium_does_not_issue_conditional_load() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-wait-ready-no-medium")
            .tempdir()
            .expect("temp dir");
        let mut index =
            remanence_state::CatalogIndex::open(temp.path().join("state.sqlite")).expect("open");
        let operation_id = Uuid::from_u128(0x9002);
        record_test_media_readiness_operation(
            &mut index,
            operation_id,
            "LIB-UA-no-medium",
            0x0100,
            "AOX030L9",
            "planned",
            Some("drive+tape"),
        );
        let (_, mut drive, log) = open_lto9_test_drive_with_tur_sequence(
            "LIB-UA-no-medium",
            "DRV-UA-no-medium",
            Path::new("/dev/sg-drive-ua-no-medium"),
            Vec::new(),
        );

        let result = poll_media_readiness_after_initial_probe(
            &mut drive,
            MediaFamily::Lto9OrLater,
            operation_id,
            &mut index,
            0x0100,
            MediaReadinessInitialProbeInput {
                initial_poll: MediaReadinessPoll {
                    readiness: MediaReadiness::NoMedium { ascq: 0x00 },
                    attempts: 1,
                    timed_out: false,
                },
                wait: true,
                timeout: StdDuration::from_secs(1),
                poll_interval: StdDuration::ZERO,
                conditional_load_on_no_medium: false,
            },
            || None,
        )
        .expect("already-loaded no-medium should terminalize without LOAD");

        assert_eq!(result.readiness, MediaReadiness::NoMedium { ascq: 0x00 });
        assert_eq!(
            log.borrow()
                .iter()
                .filter(|cdb| cdb.first().copied() == Some(0x1b))
                .count(),
            0,
            "wait-ready on already-loaded media must not issue blind LOAD IMMED"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn media_readiness_poll_clears_unit_attention_epoch_after_conditional_load() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-wait-ready-ua-epoch")
            .tempdir()
            .expect("temp dir");
        let mut index =
            remanence_state::CatalogIndex::open(temp.path().join("state.sqlite")).expect("open");
        let operation_id = Uuid::from_u128(0x9003);
        record_test_media_readiness_operation(
            &mut index,
            operation_id,
            "LIB-UA-epoch",
            0x0100,
            "AOX030L9",
            "planned",
            Some("drive+tape"),
        );
        let (_, mut drive, log) = open_lto9_test_drive_with_tur_sequence(
            "LIB-UA-epoch",
            "DRV-UA-epoch",
            Path::new("/dev/sg-drive-ua-epoch"),
            vec![
                Err(readiness_check_condition(0x02, 0x04, 0x02)),
                Err(readiness_check_condition(0x06, 0x29, 0x00)),
                Ok(()),
            ],
        );

        let result = poll_media_readiness_after_initial_probe(
            &mut drive,
            MediaFamily::Lto9OrLater,
            operation_id,
            &mut index,
            0x0100,
            MediaReadinessInitialProbeInput {
                initial_poll: MediaReadinessPoll {
                    readiness: MediaReadiness::UnitAttention {
                        asc: 0x29,
                        ascq: 0x00,
                    },
                    attempts: 1,
                    timed_out: false,
                },
                wait: true,
                timeout: StdDuration::from_secs(1),
                poll_interval: StdDuration::ZERO,
                conditional_load_on_no_medium: true,
            },
            || None,
        )
        .expect("post-load UA should start a fresh epoch and reach ready");

        assert_eq!(result.readiness, MediaReadiness::Ready);
        assert_eq!(result.attempts, 4);
        let control_cdbs = log
            .borrow()
            .iter()
            .filter(|cdb| matches!(cdb.first(), Some(0x00 | 0x1b)))
            .map(|cdb| (cdb[0], cdb[1], cdb[4]))
            .collect::<Vec<_>>();
        assert_eq!(
            control_cdbs,
            vec![
                (0x00, 0x00, 0x00),
                (0x1b, 0x01, 0x01),
                (0x00, 0x00, 0x00),
                (0x00, 0x00, 0x00)
            ]
        );
    }

    #[test]
    fn rem_daemon_health_roundtrips_against_in_process_api_service() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-daemon-health")
            .tempdir()
            .unwrap();
        let index = remanence_state::CatalogIndex::open(temp.path().join("state.sqlite")).unwrap();
        let state = remanence_api::ApiState::new(index);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let response = runtime.block_on(async move {
            let mut request = tonic::Request::new(());
            request
                .metadata_mut()
                .insert("x-remanence-role", "readonly".parse().unwrap());
            pb::daemon_server::Daemon::health(&state.daemon_service(), request)
                .await
                .expect("health should succeed")
                .into_inner()
        });

        assert_eq!(response.status, pb::health_response::Status::Healthy as i32);
        assert_eq!(
            response.components.get("sqlite_index").map(String::as_str),
            Some("ok")
        );
        assert!(
            response.detail.contains("sqlite quick_check=ok"),
            "{response:?}"
        );
    }

    #[tokio::test]
    async fn connect_daemon_unix_scheme_routes_to_unix_connector() {
        let dir = tempfile::Builder::new()
            .prefix("rem-cli-unix")
            .tempdir()
            .expect("tempdir");
        let missing = dir.path().join("nope.sock");
        let endpoint = format!("unix:{}", missing.display());
        let error = connect_daemon(&endpoint)
            .await
            .expect_err("missing unix socket must error at connect");
        assert!(
            error.starts_with("connect daemon at unix:"),
            "unix endpoint should use the UDS connector, got: {error}"
        );
    }

    // -- discovery error path ----------------------------------------

    #[test]
    fn rem_state_changing_op_points_to_rem_debug_before_discovery() {
        let cli = Cli::parse_from([
            "rem",
            "move",
            "LIB_GATE_01",
            "--src",
            "0x0400",
            "--dst",
            "0x0100",
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called when --allow gate fails")
            },
            &mut out,
            &mut err,
        );
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("available through `rem-debug`"));
        assert!(stderr.contains("normal production paths must go through the daemon"));
        assert!(out.is_empty());
    }

    #[test]
    fn rem_archive_tape_points_to_rem_debug_before_discovery() {
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "probe",
            "--format",
            "bru",
            "--tape",
            "LIB_TAPE_01",
            "--bay",
            "0x0100",
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called when archive tape --allow gate fails")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("available through `rem-debug`"));
        assert!(stderr.contains("direct local tape archive access"));
        assert!(out.is_empty());
    }

    #[test]
    fn rem_dev_write_dump_points_to_rem_debug_before_discovery() {
        let cli = Cli::parse_from([
            "rem",
            "dev",
            "write-dump-to-tape",
            "--dump",
            "/tmp/fixture.bru",
            "--tape",
            "LIB_DEV_01",
            "--bay",
            "0x0100",
            "--i-understand-this-overwrites-the-loaded-tape",
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called when dev --allow gate fails")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("available through `rem-debug`"));
        assert!(stderr.contains("development direct tape helper"));
        assert!(out.is_empty());
    }

    #[test]
    fn rem_debug_state_changing_op_without_allow_refuses_before_discovery() {
        let cli = DebugCli::parse_from([
            "rem-debug",
            "move",
            "LIB_GATE_01",
            "--src",
            "0x0400",
            "--dst",
            "0x0100",
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run_debug(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called when rem-debug --allow gate fails")
            },
            &mut out,
            &mut err,
        );
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("not on the --allow list"));
        assert!(stderr.contains("--allow LIB_GATE_01"));
        assert!(out.is_empty());
    }

    #[test]
    fn rem_debug_dev_write_dump_without_overwrite_ack_refuses_before_discovery() {
        let cli = DebugCli::parse_from([
            "rem-debug",
            "--allow",
            "LIB_DEV_01",
            "dev",
            "write-dump-to-tape",
            "--dump",
            "/tmp/fixture.bru",
            "--tape",
            "LIB_DEV_01",
            "--bay",
            "0x0100",
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run_debug(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called when destructive ack is missing")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("--i-understand-this-overwrites-the-loaded-tape"));
        assert!(out.is_empty());
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
    fn tape_block_size_parser_accepts_suffixes_and_rejects_invalid_sizes() {
        assert_eq!(parse_tape_block_size("256KiB").unwrap(), 256 * 1024);
        assert_eq!(parse_tape_block_size("512K").unwrap(), 512 * 1024);
        assert_eq!(parse_tape_block_size("1MB").unwrap(), 1024 * 1024);

        for (value, expected) in [
            ("0", "greater than zero"),
            ("1000", "multiple of 512"),
            ("17MiB", "no larger than"),
        ] {
            let err = parse_tape_block_size(value).expect_err("invalid block size");
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn tape_init_block_size_resolution_prefers_override_then_pool_then_default() {
        let config = tape_init_config_with_pool(384 * 1024);

        assert_eq!(
            resolve_tape_init_block_size(&config, "camera.copy-a", Some(512 * 1024)).unwrap(),
            512 * 1024
        );
        assert_eq!(
            resolve_tape_init_block_size(&config, "camera.copy-a", None).unwrap(),
            384 * 1024
        );
        assert_eq!(
            resolve_tape_init_block_size(&config, "missing.pool", None).unwrap(),
            remanence_state::DEFAULT_TAPE_BLOCK_SIZE_BYTES as u32
        );
    }

    #[test]
    fn planned_init_geometry_uses_fresh_block_size_but_reuses_bot_geometry() {
        let fresh_block_size = 256 * 1024;
        let (_uuid, block_size, parity) = planned_init_geometry(
            &remanence_api::BotClassification::BlankCheckEod,
            &remanence_api::InitDecision::FreshInit,
            false,
            fresh_block_size,
        );
        assert_eq!(block_size, fresh_block_size);
        assert_eq!(parity, remanence_api::ParityConfig::None);

        let existing_uuid = *Uuid::nil().as_bytes();
        let (_uuid, block_size, parity) = planned_init_geometry(
            &remanence_api::BotClassification::OursBootstrap {
                uuid: existing_uuid,
                geometry: remanence_api::TapeInitGeometry {
                    block_size_bytes: 4096,
                    parity: remanence_api::ParityConfig::None,
                },
            },
            &remanence_api::InitDecision::IdempotentNoOp,
            false,
            fresh_block_size,
        );
        assert_eq!(block_size, 4096);
        assert_eq!(parity, remanence_api::ParityConfig::None);
    }

    #[test]
    fn rebuild_catalog_command_bypasses_discovery() {
        let cli = Cli::parse_from([
            "rem",
            "rebuild-catalog-from-journals",
            "--config",
            "/no/such/rem-config.toml",
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called for catalog rebuild")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(out.is_empty());
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("read config"), "{stderr}");
    }

    #[test]
    fn catalog_reset_command_bypasses_discovery() {
        let cli = DebugCli::parse_from([
            "rem-debug",
            "catalog",
            "reset",
            "--config",
            "/no/such/rem-config.toml",
            "--i-understand-this-erases-the-catalog",
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run_debug(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called for catalog reset")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(out.is_empty());
        let stderr = String::from_utf8(err).unwrap();
        assert!(stderr.contains("read config"), "{stderr}");
    }

    #[test]
    fn catalog_reset_requires_confirmation_flag() {
        let cli = DebugCli::parse_from([
            "rem-debug",
            "catalog",
            "reset",
            "--config",
            "/no/such/rem-config.toml",
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run_debug(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called for catalog reset")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
        assert!(out.is_empty());
        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stderr.contains("--i-understand-this-erases-the-catalog"),
            "{stderr}"
        );
    }

    /// Parse + run a CLI invocation that must never reach discovery.
    fn invoke_without_discovery(argv: &[&str]) -> (ExitCode, String, String) {
        let cli = Cli::parse_from(argv);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called for this command")
            },
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).expect("stdout is utf8"),
            String::from_utf8(err).expect("stderr is utf8"),
        )
    }

    fn source_map_tsv(rows: &[(&str, &Path, &[u8], &str)]) -> String {
        let mut text = "archive_path\tsource_path\tsha256\tsize\tingest_item_id\n".to_string();
        for (archive_path, source_path, payload, ingest_item_id) in rows {
            text.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\n",
                archive_path,
                source_path.to_str().expect("test path is UTF-8"),
                bytes_to_hex(&sha256_bytes(payload)),
                payload.len(),
                ingest_item_id
            ));
        }
        text
    }

    fn write_source_map(path: &Path, rows: &[(&str, &Path, &[u8], &str)]) -> String {
        let text = source_map_tsv(rows);
        fs::write(path, &text).expect("write source map");
        bytes_to_hex(&sha256_bytes(text.as_bytes()))
    }

    fn write_test_recipient_files(
        root: &Path,
        prefix: &str,
        primary_id: u8,
    ) -> (
        remanence_aead::RecipientPrivateKey,
        PathBuf,
        PathBuf,
        PathBuf,
    ) {
        let primary = remanence_aead::RecipientPrivateKey::new(
            [primary_id; 16],
            format!("{prefix}-primary"),
            [primary_id.wrapping_add(0x20); 32],
        )
        .unwrap();
        let recovery_id = primary_id.wrapping_add(1);
        let recovery = remanence_aead::RecipientPrivateKey::new(
            [recovery_id; 16],
            format!("{prefix}-recovery"),
            [recovery_id.wrapping_add(0x20); 32],
        )
        .unwrap();
        let primary_public_path = root.join(format!("{prefix}-primary.raor"));
        let recovery_public_path = root.join(format!("{prefix}-recovery.raor"));
        let primary_private_path = root.join(format!("{prefix}-primary.raop"));
        fs::write(
            &primary_public_path,
            primary.public_key(0).unwrap().serialize().unwrap(),
        )
        .unwrap();
        fs::write(
            &recovery_public_path,
            recovery.public_key(1).unwrap().serialize().unwrap(),
        )
        .unwrap();
        fs::write(&primary_private_path, primary.serialize()).unwrap();
        (
            primary,
            primary_public_path,
            recovery_public_path,
            primary_private_path,
        )
    }

    fn default_map_archive_args(
        map: PathBuf,
        source_root: Option<PathBuf>,
        out: PathBuf,
    ) -> ArchiveBuildArgs {
        ArchiveBuildArgs {
            inputs: Vec::new(),
            map: Some(map),
            source_root,
            map_sha256: None,
            out: Some(out),
            rules: None,
            scan_only: false,
            manifest_out: None,
            no_index: false,
            blob_suggest_ratio: 0.9,
            blob_suggest_count: 100,
            sanity_ceiling_count: 10_000,
            recipients: Vec::new(),
            object_id: Some("object-map-direct".to_string()),
            caller_object_id: Some("caller-map-direct".to_string()),
            manifest_file_id: Some("manifest-map-direct".to_string()),
            timestamp: Some("2026-01-01T00:00:00Z".to_string()),
            chunk_size: 4096,
        }
    }

    fn write_retire_test_config(root: &Path) -> PathBuf {
        let config_path = root.join("config.toml");
        let toml = format!(
            r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
read_only = false

[journal]
dir = "{0}/journals"
require_trusted_volume = false

[audit]
dir = "{0}/audit"
fsync = true

[index]
sqlite_path = "{0}/index/rem-state.sqlite"

[cache]
tape_catalog_dir = "{0}/cache/tapes"
"#,
            root.display()
        );
        fs::write(&config_path, toml).expect("write retire test config");
        config_path
    }

    fn provision_retire_test_tape(config_path: &Path, tape_uuid: [u8; 16], voltag: &str) {
        let mut handle =
            remanence_state::StateHandle::open_from_config_file(config_path).expect("open state");
        handle
            .catalog_index()
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid,
                voltag: voltag.to_string(),
                block_size: 4096,
                parity: remanence_api::ParityConfig::None,
                force: false,
            })
            .expect("provision test tape");
    }

    fn retire_test_tape_state(config_path: &Path, tape_uuid: [u8; 16]) -> (String, Option<String>) {
        let config = remanence_state::load_config(config_path).expect("load config");
        let paths = remanence_state::StatePaths::from_config(config_path, &config);
        let catalog = remanence_state::CatalogIndex::open_read_only(&paths.sqlite_path)
            .expect("open catalog read-only");
        let record = catalog
            .get_tape(&tape_uuid)
            .expect("get tape")
            .expect("tape exists");
        (record.state, record.voltag)
    }

    #[test]
    fn tape_retire_parses_target_reason_ack_and_output_flags() {
        let cli = Cli::parse_from([
            "rem",
            "tape",
            "retire",
            "RMJ101L9",
            "--reason",
            "recycled",
            "--i-understand-copies-become-unreadable",
            "--config",
            "/etc/rem/config.toml",
            "--dry-run",
            "--json",
        ]);

        match cli.command {
            RemCommand::Tape {
                command: RemTapeCommand::Retire(args),
            } => {
                assert_eq!(args.target, "RMJ101L9");
                assert_eq!(args.reason, "recycled");
                assert!(args.copies_unreadable_ack);
                assert_eq!(args.config, PathBuf::from("/etc/rem/config.toml"));
                assert!(args.dry_run);
                assert!(args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn tape_alerts_parses_deprecated_drive_alerts_alias() {
        let cli = Cli::parse_from([
            "rem",
            "tape",
            "alerts",
            "DRV123",
            "--endpoint",
            "http://127.0.0.1:50051",
            "--json",
        ]);

        match cli.command {
            RemCommand::Tape {
                command: RemTapeCommand::Alerts(args),
            } => {
                assert_eq!(args.drive, "DRV123");
                assert_eq!(args.endpoint, "http://127.0.0.1:50051");
                assert!(args.json);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn tape_alerts_json_includes_flag_numbers_and_names() {
        let alerts = TapeAlerts::from_flags([20, 58]);
        let mut out = Vec::new();
        print_tape_alerts("LIB123", 0x0100, &alerts, &mut out);

        let value: serde_json::Value = serde_json::from_slice(&out).expect("alerts json");
        assert_eq!(value["schema"], "rem.tape.alerts.v1");
        assert_eq!(value["library_serial"], "LIB123");
        assert_eq!(value["bay_element"], 0x0100);
        assert_eq!(value["active"][0]["flag"], 20);
        assert_eq!(value["active"][0]["name"], "clean now");
        assert_eq!(value["active"][1]["flag"], 58);
        assert_eq!(value["active"][1]["name"], "microcode panic");
    }

    #[test]
    fn tape_retire_without_ack_exits_one_and_changes_nothing() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-retire-ack")
            .tempdir()
            .expect("temp dir");
        let config_path = write_retire_test_config(temp.path());
        let tape_uuid = [0x31u8; 16];
        provision_retire_test_tape(&config_path, tape_uuid, "RMJ101L9");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "tape",
            "retire",
            "RMJ101L9",
            "--reason",
            "recycled",
            "--config",
            config_path.to_str().unwrap(),
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(stdout.is_empty());
        assert!(
            stderr.contains("--i-understand-copies-become-unreadable"),
            "{stderr}"
        );
        let (state, voltag) = retire_test_tape_state(&config_path, tape_uuid);
        assert_eq!(state, "ready", "missing ack must not mutate the catalog");
        assert_eq!(voltag.as_deref(), Some("RMJ101L9"));
    }

    #[test]
    fn tape_retire_dry_run_reports_and_mutates_nothing() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-retire-dry")
            .tempdir()
            .expect("temp dir");
        let config_path = write_retire_test_config(temp.path());
        let tape_uuid = [0x32u8; 16];
        provision_retire_test_tape(&config_path, tape_uuid, "RMJ102L9");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "tape",
            "retire",
            "RMJ102L9",
            "--reason",
            "recycled",
            "--i-understand-copies-become-unreadable",
            "--dry-run",
            "--config",
            config_path.to_str().unwrap(),
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(
            stdout.contains("dry-run: would retire RMJ102L9"),
            "{stdout}"
        );
        assert!(stderr.is_empty(), "{stderr}");
        let (state, voltag) = retire_test_tape_state(&config_path, tape_uuid);
        assert_eq!(state, "ready", "dry-run must not mutate the catalog");
        assert_eq!(voltag.as_deref(), Some("RMJ102L9"));
    }

    #[test]
    fn tape_retire_emits_v1_json_envelope() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-retire-json")
            .tempdir()
            .expect("temp dir");
        let config_path = write_retire_test_config(temp.path());
        let tape_uuid = [0x33u8; 16];
        provision_retire_test_tape(&config_path, tape_uuid, "RMJ103L9");

        let (code, stdout, _stderr) = invoke_without_discovery(&[
            "rem",
            "tape",
            "retire",
            "RMJ103L9",
            "--reason",
            "vtl-rebuilt",
            "--i-understand-copies-become-unreadable",
            "--json",
            "--config",
            config_path.to_str().unwrap(),
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is json");
        assert_eq!(value["schema"], "rem.tape.retire.v1");
        assert_eq!(value["kind"], "item");
        assert!(value["operation"].is_null());
        assert_eq!(
            value["data"]["tape_uuid"],
            Uuid::from_bytes(tape_uuid).to_string()
        );
        assert_eq!(value["data"]["voltag"], "RMJ103L9");
        assert_eq!(value["data"]["reason"], "vtl-rebuilt");
        assert_eq!(value["data"]["dry_run"], false);
        assert_eq!(value["data"]["newly_retired"], true);
        assert_eq!(value["data"]["copies_marked_missing"], 0);
        assert_eq!(value["data"]["degraded_objects"], json!([]));
        let (state, voltag) = retire_test_tape_state(&config_path, tape_uuid);
        assert_eq!(state, "retired");
        assert_eq!(voltag, None);
    }

    #[test]
    fn tape_retire_targets_voltag_and_uuid_and_rerun_is_idempotent() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-retire-target")
            .tempdir()
            .expect("temp dir");
        let config_path = write_retire_test_config(temp.path());
        let by_voltag = [0x34u8; 16];
        let by_uuid = [0x35u8; 16];
        provision_retire_test_tape(&config_path, by_voltag, "RMJ104L9");
        provision_retire_test_tape(&config_path, by_uuid, "RMJ105L9");

        let (code, stdout, _stderr) = invoke_without_discovery(&[
            "rem",
            "tape",
            "retire",
            "RMJ104L9",
            "--reason",
            "recycled",
            "--i-understand-copies-become-unreadable",
            "--config",
            config_path.to_str().unwrap(),
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stdout.contains("retired RMJ104L9"), "{stdout}");
        assert_eq!(retire_test_tape_state(&config_path, by_voltag).0, "retired");

        // 32-hex uuid targeting.
        let uuid_hex = bytes_to_hex(&by_uuid);
        let (code, stdout, _stderr) = invoke_without_discovery(&[
            "rem",
            "tape",
            "retire",
            uuid_hex.as_str(),
            "--reason",
            "recycled",
            "--i-understand-copies-become-unreadable",
            "--config",
            config_path.to_str().unwrap(),
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stdout.contains("retired RMJ105L9"), "{stdout}");
        assert_eq!(retire_test_tape_state(&config_path, by_uuid).0, "retired");

        // Idempotent rerun by uuid (the voltag was released) succeeds with
        // the no-change shape.
        let (code, stdout, _stderr) = invoke_without_discovery(&[
            "rem",
            "tape",
            "retire",
            uuid_hex.as_str(),
            "--reason",
            "recycled",
            "--i-understand-copies-become-unreadable",
            "--config",
            config_path.to_str().unwrap(),
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stdout.contains("already retired; no change"), "{stdout}");
    }

    #[test]
    fn tape_retire_unknown_target_errors() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-retire-unknown")
            .tempdir()
            .expect("temp dir");
        let config_path = write_retire_test_config(temp.path());
        provision_retire_test_tape(&config_path, [0x36u8; 16], "RMJ106L9");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "tape",
            "retire",
            "RMJ999L9",
            "--reason",
            "recycled",
            "--i-understand-copies-become-unreadable",
            "--config",
            config_path.to_str().unwrap(),
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(stdout.is_empty());
        assert!(stderr.contains("RMJ999L9"), "{stderr}");
    }

    #[test]
    fn provision_initialized_tape_appends_tape_provisioned_audit_event() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-tape-provisioned")
            .tempdir()
            .expect("temp dir");
        let config_path = write_retire_test_config(temp.path());
        let config = remanence_state::load_config(&config_path).expect("load config");
        let tape_uuid = [0x42u8; 16];
        {
            let mut handle = remanence_state::StateHandle::open_from_config_file(&config_path)
                .expect("open state");
            TapeInitStateOps::provision_initialized_tape(
                &mut handle,
                &config,
                tape_uuid,
                "RMJ107L9".to_string(),
                4096,
                remanence_api::ParityConfig::None,
                false,
            )
            .expect("provision initialized tape");
        }

        let records =
            remanence_state::FileAuditLog::replay(temp.path().join("audit")).expect("replay");
        let provisioned = records
            .iter()
            .filter(|record| record.event == remanence_state::AuditEvent::TapeProvisioned)
            .collect::<Vec<_>>();
        assert_eq!(provisioned.len(), 1);
        let record = provisioned[0];
        assert_eq!(record.subject.kind, "tape");
        assert_eq!(
            record.subject.id.as_deref(),
            Some(bytes_to_hex(&tape_uuid).as_str())
        );
        assert_eq!(
            record.detail.get("voltag"),
            Some(&ciborium::value::Value::Text("RMJ107L9".to_string()))
        );
        assert_eq!(
            record.detail.get("block_size"),
            Some(&ciborium::value::Value::Integer(4096.into()))
        );
        assert_eq!(
            record.detail.get("geometry"),
            Some(&ciborium::value::Value::Text("no-parity".to_string()))
        );
        assert_eq!(
            record.detail.get("forced"),
            Some(&ciborium::value::Value::Bool(false))
        );
    }

    #[test]
    fn tape_init_dry_run_state_suppresses_readiness_writes_but_checks_fences() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-init-dry-run-state")
            .tempdir()
            .expect("temp dir");
        let db_path = temp.path().join("state.sqlite");
        let active_operation_id = Uuid::from_u128(0x7001);
        {
            let mut index =
                remanence_state::CatalogIndex::open(&db_path).expect("open writable catalog");
            record_test_media_readiness_operation(
                &mut index,
                active_operation_id,
                "LIB-A",
                0x0100,
                "AOX030L9",
                "media_initializing",
                Some("drive+tape"),
            );
        }
        let catalog = remanence_state::CatalogIndex::open_read_only(&db_path)
            .expect("open read-only catalog");
        let mut state = DryRunTapeInitState::new(catalog);

        let conflicts = state
            .media_readiness_admission_conflicts("LIB-A", Some(0x0100), Some("AOX030L9"), true)
            .expect("dry-run still checks active fences");
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].operation_id, active_operation_id.to_string());

        let dry_run_operation_id = Uuid::from_u128(0x7002);
        TapeInitStateOps::record_media_readiness_operation(
            &mut state,
            remanence_state::MediaReadinessOperationInput {
                operation_id: dry_run_operation_id,
                run_id: None,
                library_serial: "LIB-A".to_string(),
                changer_sg: Some("/dev/sg8".to_string()),
                drive_element: 0x0100,
                drive_sg: Some("/dev/sg7".to_string()),
                drive_serial: Some("DRV-UA-test".to_string()),
                barcode: Some("AOX031L9".to_string()),
                source_slot: Some(0x03ec),
                media_generation: Some(9),
                phase: "planned".to_string(),
                state: "planned".to_string(),
                dirty_scope: Some("drive+tape".to_string()),
                deadline_at_utc: None,
                evidence_path: None,
            },
        )
        .expect("dry-run readiness operation record is a no-op");
        TapeInitStateOps::record_media_readiness_transition(
            &mut state,
            media_readiness_signal_transition(dry_run_operation_id, "readiness_poll", "SIGINT"),
        )
        .expect("dry-run readiness transition record is a no-op");
        drop(state);

        let catalog = remanence_state::CatalogIndex::open_read_only(&db_path)
            .expect("reopen read-only catalog");
        assert!(
            catalog
                .media_readiness_operation(dry_run_operation_id)
                .expect("lookup dry-run operation")
                .is_none(),
            "dry-run readiness records must not persist"
        );
        assert!(
            catalog
                .media_readiness_operation(active_operation_id)
                .expect("lookup active operation")
                .is_some(),
            "the pre-existing active fence must remain visible"
        );
    }

    #[cfg(feature = "foreign-bru")]
    #[test]
    fn archive_probe_dump_bypasses_discovery() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-bru-probe")
            .tempdir()
            .unwrap();
        let dump = temp.path().join("fixture.bru");
        fs::write(&dump, archive_block()).unwrap();
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "probe",
            "--format",
            "bru",
            "--dump",
            dump.to_str().unwrap(),
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();

        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called for archive dump probe")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("format: remanence-bru"));
        assert!(stdout.contains("confidence: certain"));
        assert!(stdout.contains("source: byte-stream-dump"));
        assert!(err.is_empty());
    }

    #[cfg(not(feature = "foreign-bru"))]
    #[test]
    fn archive_probe_dump_reports_unavailable_without_bru_plugin() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-bru-unavailable")
            .tempdir()
            .unwrap();
        let dump = temp.path().join("fixture.bru");
        fs::write(&dump, archive_block()).unwrap();
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "probe",
            "--format",
            "bru",
            "--dump",
            dump.to_str().unwrap(),
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(stdout.is_empty());
        assert!(stderr.contains("format bru (remanence-bru) is not available in this build"));
    }

    #[test]
    fn archive_build_plaintext_bypasses_discovery_and_round_trips() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-build-plain")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("nested")).unwrap();
        fs::write(input_dir.join("alpha.txt"), b"alpha\n").unwrap();
        fs::write(input_dir.join("nested/beta.bin"), [0xB7u8; 6000]).unwrap();
        let out_path = temp.path().join("plain.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-plain",
            "--caller-object-id",
            "caller-plain",
            "--manifest-file-id",
            "manifest-plain",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        assert!(out_path.exists());

        let report: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is json");
        assert_eq!(report["object_id"], "object-plain");
        assert_eq!(report["caller_object_id"], "caller-plain");
        assert_eq!(report["body_format"], "rao-v1");
        assert_eq!(report["representation"], "plaintext");
        assert_eq!(report["encryption"], "none");
        assert_eq!(report["chunk_size"], 4096);
        assert_eq!(report["stored_digest"], report["plaintext_digest"]);
        let files = report["files"].as_array().expect("files array");
        assert_eq!(files.len(), 2);
        assert_eq!(files[0]["path"], "alpha.txt");
        assert_eq!(
            files[0]["file_sha256"],
            bytes_to_hex(&sha256_bytes(b"alpha\n"))
        );
        assert_eq!(files[1]["path"], "nested/beta.bin");

        let mut source = remanence_library::FileBlockSource::open(&out_path, 4096).unwrap();
        let block_count = source.block_count();
        assert_eq!(report["stored_size_blocks"].as_u64().unwrap(), block_count);
        let read = remanence_format::read_rem_tar_object(&mut source, 4096, block_count).unwrap();
        assert_eq!(read.entry("alpha.txt").unwrap().data, b"alpha\n");
        assert_eq!(read.entry("nested/beta.bin").unwrap().data, &[0xB7u8; 6000]);

        let second_out_path = temp.path().join("plain-second.rao");
        let (code, _stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--out",
            second_out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-plain",
            "--caller-object-id",
            "caller-plain",
            "--manifest-file-id",
            "manifest-plain",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        assert_eq!(
            fs::read(&out_path).unwrap(),
            fs::read(&second_out_path).unwrap()
        );
    }

    #[test]
    fn archive_build_map_round_trips_preserves_order_and_reports_ids() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-build-map")
            .tempdir()
            .unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(source_root.join("camera")).unwrap();
        fs::create_dir_all(source_root.join("docs")).unwrap();
        let zeta_path = source_root.join("camera/zeta.bin");
        let alpha_path = source_root.join("alpha.txt");
        let beta_path = source_root.join("docs/beta.txt");
        let zeta_payload = b"zeta payload\n";
        let alpha_payload = b"alpha payload\n";
        let beta_payload = b"beta payload\n";
        fs::write(&zeta_path, zeta_payload).unwrap();
        fs::write(&alpha_path, alpha_payload).unwrap();
        fs::write(&beta_path, beta_payload).unwrap();
        let map_path = temp.path().join("source-map.tsv");
        let rows: Vec<(&str, &Path, &[u8], &str)> = vec![
            (
                "z/zeta.bin",
                zeta_path.as_path(),
                &zeta_payload[..],
                "ingest-z",
            ),
            (
                "a.txt",
                alpha_path.as_path(),
                &alpha_payload[..],
                "ingest-a",
            ),
            (
                "nested/beta.txt",
                beta_path.as_path(),
                &beta_payload[..],
                "ingest-beta",
            ),
        ];
        let map_hash = write_source_map(&map_path, &rows);
        let map_hash_arg = format!("{map_hash}  source-map.tsv");
        let out_path = temp.path().join("map.rao");
        let manifest_path = temp.path().join("map-manifest.json");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--map",
            map_path.to_str().unwrap(),
            "--source-root",
            source_root.to_str().unwrap(),
            "--map-sha256",
            map_hash_arg.as_str(),
            "--manifest-out",
            manifest_path.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-map",
            "--caller-object-id",
            "caller-map",
            "--manifest-file-id",
            "manifest-map",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        assert!(out_path.exists());
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        assert_eq!(report["map_sha256"], map_hash);
        assert!(report.get("ingest").is_none());
        assert_eq!(
            report["manifest_out"].as_str(),
            Some(manifest_path.to_str().unwrap())
        );
        let files = report["files"].as_array().expect("files array");
        let paths = files
            .iter()
            .map(|file| file["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["z/zeta.bin", "a.txt", "nested/beta.txt"]);
        let ingest_ids = files
            .iter()
            .map(|file| file["ingest_item_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ingest_ids, vec!["ingest-z", "ingest-a", "ingest-beta"]);
        let lbas = files
            .iter()
            .map(|file| file["first_chunk_lba"].as_u64().unwrap())
            .collect::<Vec<_>>();
        assert!(
            lbas.windows(2).all(|window| window[0] < window[1]),
            "{lbas:?}"
        );

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["format"], "remanence-customer-manifest-v1");
        assert_eq!(manifest["tar_engine"]["program"], "source-map");
        let manifest_paths = manifest["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(manifest_paths, paths);

        let mut source = remanence_library::FileBlockSource::open(&out_path, 4096).unwrap();
        let block_count = source.block_count();
        let read = remanence_format::read_rem_tar_object(&mut source, 4096, block_count).unwrap();
        assert_eq!(read.entry("z/zeta.bin").unwrap().data, zeta_payload);
        assert_eq!(read.entry("a.txt").unwrap().data, alpha_payload);
        assert_eq!(read.entry("nested/beta.txt").unwrap().data, beta_payload);
        assert!(files
            .iter()
            .all(|file| !file["path"].as_str().unwrap().contains(".remwrap.")));
    }

    #[test]
    fn archive_build_map_rejects_malformed_rows_and_duplicate_paths() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-map-malformed")
            .tempdir()
            .unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let source_path = source_root.join("good.txt");
        let payload = b"good payload";
        fs::write(&source_path, payload).unwrap();
        let source = source_path.to_str().unwrap();
        let good_hash = bytes_to_hex(&sha256_bytes(payload));
        let good_size = payload.len();
        let header = "archive_path\tsource_path\tsha256\tsize\tingest_item_id\n";
        let valid_row = format!("good.txt\t{source}\t{good_hash}\t{good_size}\tid\n");
        let cases = [
            (
                "wrong-columns",
                format!("{header}only\tfour\tcolumns\tbad\n"),
                "must have 5 TAB-separated columns",
            ),
            (
                "control",
                format!("{header}bad.txt\t{source}\t{good_hash}\t{good_size}\tid\u{1F}\n"),
                "control character",
            ),
            (
                "bad-hex",
                format!(
                    "{header}bad-hex.txt\t{source}\t{}\t{good_size}\tid\n",
                    "A".repeat(64)
                ),
                "sha256 must use lowercase hex",
            ),
            (
                "bad-size",
                format!("{header}bad-size.txt\t{source}\t{good_hash}\tnope\tid\n"),
                "size",
            ),
            (
                "duplicate",
                format!("{header}{valid_row}{valid_row}"),
                "duplicate archive path",
            ),
        ];

        for (name, map_text, expected) in cases {
            let map_path = temp.path().join(format!("{name}.tsv"));
            let out_path = temp.path().join(format!("{name}.rao"));
            fs::write(&map_path, map_text).unwrap();

            let (code, stdout, stderr) = invoke_without_discovery(&[
                "rem",
                "archive",
                "build",
                "--map",
                map_path.to_str().unwrap(),
                "--source-root",
                source_root.to_str().unwrap(),
                "--out",
                out_path.to_str().unwrap(),
                "--chunk-size",
                "4KiB",
                "--object-id",
                "object-map-malformed",
                "--caller-object-id",
                "caller-map-malformed",
                "--manifest-file-id",
                "manifest-map-malformed",
                "--timestamp",
                "2026-01-01T00:00:00Z",
            ]);

            assert_eq!(
                format!("{code:?}"),
                format!("{:?}", ExitCode::from(1)),
                "{name}: {stdout} {stderr}"
            );
            assert!(stdout.is_empty(), "{name}: {stdout}");
            assert!(stderr.contains(expected), "{name}: {stderr}");
            assert!(!out_path.exists(), "{name} must not leave an object");
        }

        let mut non_utf8 = header.as_bytes().to_vec();
        non_utf8.extend_from_slice(b"\xff\t");
        non_utf8.extend_from_slice(source.as_bytes());
        non_utf8.extend_from_slice(b"\t");
        non_utf8.extend_from_slice(good_hash.as_bytes());
        non_utf8.extend_from_slice(format!("\t{good_size}\tid\n").as_bytes());
        let byte_cases = [
            (
                "bad-header",
                format!("archive_path\tsource_path\tsha256\tsize\twrong\n{valid_row}").into_bytes(),
                "header must be exactly",
            ),
            (
                "missing-newline",
                format!("{header}{}", valid_row.trim_end_matches('\n')).into_bytes(),
                "trailing LF newline",
            ),
            ("non-utf8", non_utf8, "not UTF-8"),
        ];
        for (name, map_bytes, expected) in byte_cases {
            let map_path = temp.path().join(format!("{name}.tsv"));
            let out_path = temp.path().join(format!("{name}.rao"));
            fs::write(&map_path, map_bytes).unwrap();

            let (code, stdout, stderr) = invoke_without_discovery(&[
                "rem",
                "archive",
                "build",
                "--map",
                map_path.to_str().unwrap(),
                "--source-root",
                source_root.to_str().unwrap(),
                "--out",
                out_path.to_str().unwrap(),
                "--chunk-size",
                "4KiB",
            ]);

            assert_eq!(
                format!("{code:?}"),
                format!("{:?}", ExitCode::from(1)),
                "{name}: {stdout} {stderr}"
            );
            assert!(stdout.is_empty(), "{name}: {stdout}");
            assert!(stderr.contains(expected), "{name}: {stderr}");
            assert!(!out_path.exists(), "{name} must not leave an object");
        }
    }

    #[test]
    fn archive_build_map_rejects_raw_archive_paths_without_normalizing() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-map-paths")
            .tempdir()
            .unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let source_path = source_root.join("good.txt");
        let payload = b"path payload";
        fs::write(&source_path, payload).unwrap();
        let source = source_path.to_str().unwrap();
        let hash = bytes_to_hex(&sha256_bytes(payload));
        let size = payload.len();
        let bad_paths = ["/abs", "a//b", "a/./b", "a/../b", "a/"];

        for (index, archive_path) in bad_paths.iter().enumerate() {
            let map_path = temp.path().join(format!("bad-path-{index}.tsv"));
            let out_path = temp.path().join(format!("bad-path-{index}.rao"));
            fs::write(
                &map_path,
                format!(
                    "archive_path\tsource_path\tsha256\tsize\tingest_item_id\n{archive_path}\t{source}\t{hash}\t{size}\tid\n"
                ),
            )
            .unwrap();

            let (code, stdout, stderr) = invoke_without_discovery(&[
                "rem",
                "archive",
                "build",
                "--map",
                map_path.to_str().unwrap(),
                "--source-root",
                source_root.to_str().unwrap(),
                "--out",
                out_path.to_str().unwrap(),
                "--chunk-size",
                "4KiB",
            ]);

            assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
            assert!(stdout.is_empty());
            assert!(stderr.contains("archive_path"), "{archive_path}: {stderr}");
            assert!(!out_path.exists());
        }
    }

    #[test]
    fn archive_build_map_rejects_source_escape_and_relative_source() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-map-source")
            .tempdir()
            .unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let inside = source_root.join("inside.txt");
        let outside = temp.path().join("outside.txt");
        let payload = b"escape payload";
        fs::write(&inside, payload).unwrap();
        fs::write(&outside, payload).unwrap();
        let hash = bytes_to_hex(&sha256_bytes(payload));
        let size = payload.len();

        let cases = [
            (
                "outside",
                format!(
                    "archive_path\tsource_path\tsha256\tsize\tingest_item_id\noutside.txt\t{}\t{hash}\t{size}\tid\n",
                    outside.to_str().unwrap()
                ),
                "escapes --source-root",
            ),
            (
                "relative",
                format!(
                    "archive_path\tsource_path\tsha256\tsize\tingest_item_id\nrelative.txt\tinside.txt\t{hash}\t{size}\tid\n"
                ),
                "must be absolute",
            ),
        ];
        for (name, map_text, expected) in cases {
            let map_path = temp.path().join(format!("{name}.tsv"));
            let out_path = temp.path().join(format!("{name}.rao"));
            fs::write(&map_path, map_text).unwrap();
            let (code, stdout, stderr) = invoke_without_discovery(&[
                "rem",
                "archive",
                "build",
                "--map",
                map_path.to_str().unwrap(),
                "--source-root",
                source_root.to_str().unwrap(),
                "--out",
                out_path.to_str().unwrap(),
                "--chunk-size",
                "4KiB",
            ]);
            assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
            assert!(stdout.is_empty());
            assert!(stderr.contains(expected), "{name}: {stderr}");
            assert!(!out_path.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_map_rejects_symlink_escape_before_reading() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-map-symlink")
            .tempdir()
            .unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let outside = temp.path().join("outside.txt");
        let link = source_root.join("link.txt");
        let payload = b"outside payload";
        fs::write(&outside, payload).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let map_path = temp.path().join("symlink.tsv");
        fs::write(
            &map_path,
            format!(
                "archive_path\tsource_path\tsha256\tsize\tingest_item_id\nlink.txt\t{}\t{}\t{}\tid\n",
                link.to_str().unwrap(),
                bytes_to_hex(&sha256_bytes(payload)),
                payload.len()
            ),
        )
        .unwrap();
        let out_path = temp.path().join("symlink.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--map",
            map_path.to_str().unwrap(),
            "--source-root",
            source_root.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(stdout.is_empty());
        assert!(stderr.contains("escapes --source-root"), "{stderr}");
        assert!(!out_path.exists());
    }

    #[test]
    fn archive_build_map_rejects_guard_errors_before_tsv_read() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-map-guards")
            .tempdir()
            .unwrap();
        let missing_map = temp.path().join("missing.tsv");
        let out_path = temp.path().join("missing.rao");

        let args = default_map_archive_args(missing_map.clone(), None, out_path.clone());
        let error = build_archive_object_file(&args).unwrap_err();
        assert!(error.contains("--map requires --source-root"), "{error}");
        assert!(!out_path.exists());

        let mut scan_args = default_map_archive_args(
            missing_map.clone(),
            Some(temp.path().to_path_buf()),
            out_path.clone(),
        );
        scan_args.scan_only = true;
        let error = build_archive_object_file(&scan_args).unwrap_err();
        assert!(error.contains("--scan-only"), "{error}");
        assert!(!out_path.exists());

        let parse_error = Cli::try_parse_from([
            "rem",
            "archive",
            "build",
            "--map",
            missing_map.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
        ])
        .unwrap_err()
        .to_string();
        assert!(parse_error.contains("--source-root"), "{parse_error}");

        let conflict_error = Cli::try_parse_from([
            "rem",
            "archive",
            "build",
            "--map",
            missing_map.to_str().unwrap(),
            "--source-root",
            temp.path().to_str().unwrap(),
            "--scan-only",
        ])
        .unwrap_err()
        .to_string();
        assert!(conflict_error.contains("--map"), "{conflict_error}");
        assert!(conflict_error.contains("--scan-only"), "{conflict_error}");
    }

    #[test]
    fn archive_build_map_sha256_mismatch_fails_before_source_validation() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-map-mapsha")
            .tempdir()
            .unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let missing_source = source_root.join("missing.txt");
        let map_path = temp.path().join("source-map.tsv");
        fs::write(
            &map_path,
            format!(
                "archive_path\tsource_path\tsha256\tsize\tingest_item_id\nmissing.txt\t{}\t{}\t1\tid\n",
                missing_source.to_str().unwrap(),
                "0".repeat(64)
            ),
        )
        .unwrap();
        let out_path = temp.path().join("mapsha.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--map",
            map_path.to_str().unwrap(),
            "--source-root",
            source_root.to_str().unwrap(),
            "--map-sha256",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(stdout.is_empty());
        assert!(stderr.contains("--map-sha256 mismatch"), "{stderr}");
        assert!(!stderr.contains("canonicalize source_path"), "{stderr}");
        assert!(!out_path.exists());
    }

    #[test]
    fn archive_build_map_rejects_size_and_payload_hash_mismatches_closed() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-map-mismatch")
            .tempdir()
            .unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let source_path = source_root.join("payload.txt");
        let payload = b"actual payload";
        fs::write(&source_path, payload).unwrap();
        let source = source_path.to_str().unwrap();
        let hash = bytes_to_hex(&sha256_bytes(payload));

        let size_map = temp.path().join("size.tsv");
        let size_out = temp.path().join("size.rao");
        fs::write(
            &size_map,
            format!(
                "archive_path\tsource_path\tsha256\tsize\tingest_item_id\npayload.txt\t{source}\t{hash}\t{}\tid\n",
                payload.len() + 1
            ),
        )
        .unwrap();
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--map",
            size_map.to_str().unwrap(),
            "--source-root",
            source_root.to_str().unwrap(),
            "--out",
            size_out.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(stdout.is_empty());
        assert!(stderr.contains("size mismatch"), "{stderr}");
        assert!(!size_out.exists());

        let hash_map = temp.path().join("hash.tsv");
        let hash_out = temp.path().join("hash.rao");
        fs::write(
            &hash_map,
            format!(
                "archive_path\tsource_path\tsha256\tsize\tingest_item_id\npayload.txt\t{source}\t{}\t{}\tid\n",
                "0".repeat(64),
                payload.len()
            ),
        )
        .unwrap();
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--map",
            hash_map.to_str().unwrap(),
            "--source-root",
            source_root.to_str().unwrap(),
            "--out",
            hash_out.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(stdout.is_empty());
        assert!(stderr.contains("streamed data hash"), "{stderr}");
        assert!(!hash_out.exists());
    }

    #[test]
    fn archive_build_map_encrypted_round_trips() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-map-encrypted")
            .tempdir()
            .unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let source_path = source_root.join("secret.txt");
        let payload = b"map encrypted payload";
        fs::write(&source_path, payload).unwrap();
        let map_path = temp.path().join("source-map.tsv");
        let rows: Vec<(&str, &Path, &[u8], &str)> = vec![(
            "secret.txt",
            source_path.as_path(),
            &payload[..],
            "secret-id",
        )];
        write_source_map(&map_path, &rows);
        let (primary, primary_public, recovery_public, primary_private) =
            write_test_recipient_files(temp.path(), "map", 0x5a);
        let out_path = temp.path().join("map-encrypted.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--map",
            map_path.to_str().unwrap(),
            "--source-root",
            source_root.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--recipient",
            primary_public.to_str().unwrap(),
            "--recipient",
            recovery_public.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-map-encrypted",
            "--caller-object-id",
            "caller-map-encrypted",
            "--manifest-file-id",
            "manifest-map-encrypted",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        assert_eq!(report["representation"], "encrypted");
        assert_eq!(report["files"][0]["ingest_item_id"], "secret-id");

        let mut source = remanence_library::FileBlockSource::open(&out_path, 4096).unwrap();
        let block_count = source.block_count();
        let read =
            remanence_format::read_encrypted_rao_object(&mut source, 4096, block_count, &primary)
                .unwrap();
        assert_eq!(read.object.entry("secret.txt").unwrap().data, payload);

        let restore_dir = temp.path().join("restore");
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            restore_dir.to_str().unwrap(),
            "--private-key",
            primary_private.to_str().unwrap(),
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let extract: serde_json::Value = serde_json::from_str(&stdout).expect("extract json");
        assert_eq!(extract["representation"], "encrypted");
        assert_eq!(fs::read(restore_dir.join("secret.txt")).unwrap(), payload);
    }

    #[test]
    fn archive_build_map_is_byte_reproducible_with_pinned_ids() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-map-repro")
            .tempdir()
            .unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(&source_root).unwrap();
        let source_path = source_root.join("asset.bin");
        let payload = b"repro payload";
        fs::write(&source_path, payload).unwrap();
        let map_path = temp.path().join("source-map.tsv");
        let rows: Vec<(&str, &Path, &[u8], &str)> =
            vec![("asset.bin", source_path.as_path(), &payload[..], "asset-id")];
        write_source_map(&map_path, &rows);
        let first_out = temp.path().join("first.rao");
        let second_out = temp.path().join("second.rao");

        for out_path in [&first_out, &second_out] {
            let (code, _stdout, stderr) = invoke_without_discovery(&[
                "rem",
                "archive",
                "build",
                "--map",
                map_path.to_str().unwrap(),
                "--source-root",
                source_root.to_str().unwrap(),
                "--out",
                out_path.to_str().unwrap(),
                "--chunk-size",
                "4KiB",
                "--object-id",
                "object-map-repro",
                "--caller-object-id",
                "caller-map-repro",
                "--manifest-file-id",
                "manifest-map-repro",
                "--timestamp",
                "2026-01-01T00:00:00Z",
            ]);
            assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
            assert!(stderr.is_empty(), "{stderr}");
        }

        assert_eq!(
            fs::read(&first_out).unwrap(),
            fs::read(&second_out).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_plaintext_preserves_symlinks_and_empty_directories() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-build-nonregular")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("empty")).unwrap();
        fs::write(input_dir.join("target.txt"), b"target").unwrap();
        std::os::unix::fs::symlink("target.txt", input_dir.join("latest")).unwrap();
        std::os::unix::fs::symlink("missing.txt", input_dir.join("dangling")).unwrap();
        let out_path = temp.path().join("nonregular.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-nonregular",
            "--caller-object-id",
            "caller-nonregular",
            "--manifest-file-id",
            "manifest-nonregular",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");

        let report: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is json");
        let files = report["files"].as_array().expect("files array");
        assert_eq!(files.len(), 4);
        let empty = files.iter().find(|file| file["path"] == "empty/").unwrap();
        assert_eq!(empty["entry_type"], "directory");
        assert!(empty["file_sha256"].is_null());
        let dangling = files
            .iter()
            .find(|file| file["path"] == "dangling")
            .unwrap();
        assert_eq!(dangling["entry_type"], "symlink");
        assert_eq!(dangling["link_target"], "missing.txt");
        assert!(dangling["file_sha256"].is_null());

        let mut source = remanence_library::FileBlockSource::open(&out_path, 4096).unwrap();
        let block_count = source.block_count();
        let read = remanence_format::read_rem_tar_object(&mut source, 4096, block_count).unwrap();

        let empty = read.entry("empty/").unwrap();
        assert_eq!(empty.entry_type, RemTarEntryType::Directory);
        assert_eq!(empty.first_chunk_lba, None);
        assert!(empty.data.is_empty());

        let latest = read.entry("latest").unwrap();
        assert_eq!(latest.entry_type, RemTarEntryType::Symlink);
        assert_eq!(latest.link_target.as_deref(), Some("target.txt"));
        assert!(latest.data.is_empty());

        let dangling = read.entry("dangling").unwrap();
        assert_eq!(dangling.entry_type, RemTarEntryType::Symlink);
        assert_eq!(dangling.link_target.as_deref(), Some("missing.txt"));
        assert!(dangling.data.is_empty());
        assert_eq!(read.entry("target.txt").unwrap().data, b"target");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "inspect",
            "--object",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let inspect: serde_json::Value = serde_json::from_str(&stdout).expect("inspect json");
        assert_eq!(inspect["representation"], "plaintext");
        assert_eq!(inspect["object_id"], "object-nonregular");
        let inspected_files = inspect["files"].as_array().expect("inspect files");
        assert_eq!(inspected_files.len(), 4);
        let inspected_latest = inspected_files
            .iter()
            .find(|file| file["path"] == "latest")
            .unwrap();
        assert_eq!(inspected_latest["entry_type"], "symlink");
        assert_eq!(inspected_latest["link_target"], "target.txt");

        let restore_dir = temp.path().join("restore");
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            restore_dir.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let extract: serde_json::Value = serde_json::from_str(&stdout).expect("extract json");
        assert_eq!(extract["representation"], "plaintext");
        assert_eq!(extract["files_written"], 1);
        assert_eq!(extract["directories_seen"], 1);
        assert_eq!(extract["symlinks_written"], 2);
        assert_eq!(extract["hardlinks_written"], 0);
        assert_eq!(extract["bytes_written"], 6);
        assert_eq!(fs::read(restore_dir.join("target.txt")).unwrap(), b"target");
        assert!(restore_dir.join("empty").is_dir());
        assert!(fs::symlink_metadata(restore_dir.join("latest"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(restore_dir.join("latest")).unwrap(),
            PathBuf::from("target.txt")
        );
        assert_eq!(
            fs::read_link(restore_dir.join("dangling")).unwrap(),
            PathBuf::from("missing.txt")
        );

        let top_level_restore_dir = temp.path().join("top-level-restore");
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "restore",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            top_level_restore_dir.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let restore: serde_json::Value = serde_json::from_str(&stdout).expect("restore json");
        assert_eq!(restore["representation"], "plaintext");
        assert_eq!(restore["files_written"], 1);
        assert_eq!(
            fs::read(top_level_restore_dir.join("target.txt")).unwrap(),
            b"target"
        );
    }

    #[test]
    fn plaintext_locator_scan_resolves_hardlink_pfr_rows() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-hardlink-pfr")
            .tempdir()
            .unwrap();
        let object_path = temp.path().join("hardlinks.rao");
        let primary = b"shared hardlink payload".to_vec();
        let mut primary_reader = Cursor::new(primary.as_slice());
        let mut hardlink_reader = io::empty();
        let mut streams = [
            RemTarFileStream::new(
                RemTarFileSpec::new(
                    "primary.txt",
                    "file-primary",
                    primary.len() as u64,
                    sha256_bytes(&primary),
                ),
                &mut primary_reader,
            ),
            RemTarFileStream::new(
                RemTarFileSpec::hardlink("links/copy.txt", "hardlink-copy", "primary.txt"),
                &mut hardlink_reader,
            ),
        ];
        let mut sink = remanence_library::VecBlockSink::new();
        let mut options = RemTarObjectOptions::new(
            "object-hardlinks",
            "caller-hardlinks",
            "2026-01-01T00:00:00Z",
            "manifest-hardlinks",
        );
        options.chunk_size = 4096;
        write_rem_tar_object_from_readers(&mut sink, &options, &mut streams).unwrap();
        let bytes = sink.blocks.iter().flatten().copied().collect::<Vec<_>>();
        fs::write(&object_path, &bytes).unwrap();

        let scan = scan_rao_entry_locators_from_bytes(&bytes, 4096).unwrap();
        let hardlink = scan
            .entries
            .iter()
            .find(|entry| entry.path == "links/copy.txt")
            .unwrap();
        assert_eq!(hardlink.entry_type, RemTarEntryType::Hardlink);
        assert_eq!(hardlink.link_target.as_deref(), Some("primary.txt"));
        assert_eq!(hardlink.first_chunk_lba, None);

        let resolved = require_regular_locator(&scan, "links/copy.txt").unwrap();
        assert_eq!(resolved.path, "primary.txt");
        assert_eq!(resolved.size_bytes, primary.len() as u64);
        let bytes = read_plaintext_rao_entry_range(&object_path, resolved, 7, 8).unwrap();
        assert_eq!(bytes, b"hardlink");
    }

    #[test]
    fn archive_build_with_rules_blobs_indexes_manifests_and_unwraps() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-build-rules")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("Project/Render Files")).unwrap();
        fs::create_dir_all(input_dir.join("Cache")).unwrap();
        fs::write(input_dir.join("keep.mov"), b"deliverable").unwrap();
        fs::write(
            input_dir.join("Project/Render Files/frame.dat"),
            b"render-frame",
        )
        .unwrap();
        fs::write(input_dir.join("Cache/drop.tmp"), b"cache").unwrap();
        let rules = temp.path().join("fcp.rules");
        fs::write(
            &rules,
            "\
exclude Cache/
blob Project/Render Files/
",
        )
        .unwrap();
        let out_path = temp.path().join("rules.rao");
        let manifest_path = temp.path().join("customer-manifest.json");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--manifest-out",
            manifest_path.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-rules",
            "--caller-object-id",
            "caller-rules",
            "--manifest-file-id",
            "manifest-rules",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        assert_eq!(report["object_id"], "object-rules");
        assert_eq!(report["ingest"]["scan"]["totals"]["blob_entries"], 1);
        assert_eq!(report["ingest"]["scan"]["totals"]["excluded_entries"], 2);
        let files = report["files"].as_array().expect("files array");
        assert!(files
            .iter()
            .any(|file| file["path"] == "Project/Render Files.remwrap.tar"));
        assert!(files
            .iter()
            .any(|file| file["path"] == "Project/Render Files.remwrap.idx"));
        assert!(files.iter().any(|file| file["path"] == "keep.mov"));
        assert!(!files.iter().any(|file| file["path"] == "Cache/drop.tmp"));

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["format"], "remanence-customer-manifest-v1");
        assert!(manifest["entries"].as_array().unwrap().iter().any(|entry| {
            entry["path"] == "Project/Render Files/frame.dat"
                && entry["sha256"] == bytes_to_hex(&sha256_bytes(b"render-frame"))
                && entry["mtime"].as_str().is_some()
        }));
        assert_eq!(manifest["exclusions"][0]["reason"], "exclude-rule");

        let mut source = remanence_library::FileBlockSource::open(&out_path, 4096).unwrap();
        let block_count = source.block_count();
        let read = remanence_format::read_rem_tar_object(&mut source, 4096, block_count).unwrap();
        let idx = read.entry("Project/Render Files.remwrap.idx").unwrap();
        let index: serde_json::Value = serde_json::from_slice(&idx.data).unwrap();
        assert_eq!(index["format"], "remanence-remwrap-idx-v1");
        assert!(index["entries"].as_array().unwrap().iter().any(|entry| {
            entry["path"] == "Project/Render Files/frame.dat"
                && entry["sha256"] == bytes_to_hex(&sha256_bytes(b"render-frame"))
                && entry["mtime"].as_str().is_some()
        }));

        let restore_dir = temp.path().join("restore");
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            restore_dir.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let extract: serde_json::Value = serde_json::from_str(&stdout).expect("extract json");
        assert_eq!(extract["unwrap"]["enabled"], true);
        assert_eq!(extract["unwrap"]["wrappers_unwrapped"], 1);
        assert_eq!(
            fs::read(restore_dir.join("keep.mov")).unwrap(),
            b"deliverable"
        );
        assert_eq!(
            fs::read(restore_dir.join("Project/Render Files/frame.dat")).unwrap(),
            b"render-frame"
        );
        assert!(!restore_dir
            .join("Project/Render Files.remwrap.tar")
            .exists());
        assert!(!restore_dir
            .join("Project/Render Files.remwrap.idx")
            .exists());

        let literal_dir = temp.path().join("literal-restore");
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            literal_dir.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--no-unwrap",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let literal: serde_json::Value = serde_json::from_str(&stdout).expect("literal json");
        assert_eq!(literal["unwrap"]["enabled"], false);
        assert!(literal_dir
            .join("Project/Render Files.remwrap.tar")
            .exists());
        assert!(literal_dir
            .join("Project/Render Files.remwrap.idx")
            .exists());
        assert!(!literal_dir.join("Project/Render Files/frame.dat").exists());

        let member_dir = temp.path().join("member-restore");
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            member_dir.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--blob-entry",
            "Project/Render Files.remwrap.tar",
            "--blob-member",
            "Project/Render Files/frame.dat",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let member: serde_json::Value = serde_json::from_str(&stdout).expect("member json");
        assert_eq!(member["mode"], "blob-member");
        assert_eq!(member["range_method"], "rao-entry-range");
        assert_eq!(member["idx_entry"], "Project/Render Files.remwrap.idx");
        assert_eq!(member["blob_range_len"], 12);
        assert!(member["blob_first_chunk_lba"].is_number());
        assert!(member["idx_first_chunk_lba"].is_number());
        assert!(member["idx_stored_range_start"].is_number());
        assert!(member["blob_stored_range_start"].is_number());
        assert_eq!(member["bytes_written"], 12);
        assert_eq!(
            fs::read(member_dir.join("Project/Render Files/frame.dat")).unwrap(),
            b"render-frame"
        );
    }

    #[test]
    fn archive_build_rules_scan_only_does_not_require_out() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-scan-only")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("messy")).unwrap();
        fs::write(input_dir.join("messy/a.bin"), b"a").unwrap();
        let rules = temp.path().join("scan.rules");
        fs::write(&rules, "blob messy/\n").unwrap();

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--scan-only",
            "--blob-suggest-count",
            "1",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("scan json");
        assert_eq!(report["ruleset"]["name"], "scan");
        assert_eq!(report["scan"]["totals"]["blob_entries"], 1);
        assert!(!temp.path().join("archive.rao").exists());
    }

    #[test]
    fn archive_build_scan_only_accepts_inputs_without_rules() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-scan-only-no-rules")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("native")).unwrap();
        fs::write(input_dir.join("native/payload.bin"), b"native").unwrap();

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--scan-only",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("scan json");
        assert!(report["ruleset"].is_null());
        assert_eq!(report["scan"]["totals"]["native_entries"], 1);
        assert_eq!(report["scan"]["totals"]["blob_entries"], 0);
        assert_eq!(report["scan"]["totals"]["wrapped_entries"], 0);
    }

    #[test]
    fn archive_build_rules_scan_only_matches_build_verdicts() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-scan-parity")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("messy")).unwrap();
        fs::write(input_dir.join("native.txt"), b"native").unwrap();
        fs::write(input_dir.join("messy/blobbed.bin"), b"blobbed").unwrap();
        let rules = temp.path().join("scan.rules");
        fs::write(&rules, "blob messy/\n").unwrap();

        let (scan_code, scan_stdout, scan_stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--scan-only",
        ]);
        assert_eq!(format!("{scan_code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(scan_stderr.is_empty(), "{scan_stderr}");

        let out_path = temp.path().join("scan-parity.rao");
        let (build_code, build_stdout, build_stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-scan-parity",
            "--caller-object-id",
            "caller-scan-parity",
            "--manifest-file-id",
            "manifest-scan-parity",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);
        assert_eq!(
            format!("{build_code:?}"),
            format!("{:?}", ExitCode::SUCCESS)
        );
        assert!(build_stderr.is_empty(), "{build_stderr}");

        let scan: serde_json::Value = serde_json::from_str(&scan_stdout).expect("scan json");
        let build: serde_json::Value = serde_json::from_str(&build_stdout).expect("build json");
        let scan_report = &scan["scan"];
        let build_report = &build["ingest"]["scan"];
        for key in [
            "native_entries",
            "wrapped_entries",
            "blob_entries",
            "excluded_entries",
            "dropped_xattrs",
        ] {
            assert_eq!(
                scan_report["totals"][key], build_report["totals"][key],
                "scan/build total mismatch for {key}"
            );
        }
        assert_eq!(
            cluster_verdicts(scan_report),
            cluster_verdicts(build_report),
            "scan/build classification clusters differ"
        );
    }

    fn cluster_verdicts(scan_report: &serde_json::Value) -> Vec<(String, String, u64)> {
        let mut verdicts = scan_report["clusters"]
            .as_array()
            .expect("clusters array")
            .iter()
            .map(|cluster| {
                (
                    cluster["prefix"].as_str().unwrap().to_string(),
                    cluster["reason"].as_str().unwrap().to_string(),
                    cluster["count"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        verdicts.sort();
        verdicts
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_rules_scan_only_does_not_read_file_contents() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-scan-nohash")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        let file = input_dir.join("unreadable.bin");
        fs::write(&file, b"scan must not hash this payload").unwrap();
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&file, permissions).unwrap();
        if fs::File::open(&file).is_ok() {
            let mut permissions = fs::metadata(&file).unwrap().permissions();
            permissions.set_mode(0o600);
            fs::set_permissions(&file, permissions).unwrap();
            return;
        }

        let report = archive_ingest::scan_only_report(
            std::slice::from_ref(&input_dir),
            None,
            false,
            archive_ingest::ScanTuning::default(),
        )
        .expect("scan-only does not open regular-file payloads");
        let mut permissions = fs::metadata(&file).unwrap().permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&file, permissions).unwrap();

        assert_eq!(report.scan.totals.native_entries, 1);
        assert_eq!(report.scan.totals.wrapped_entries, 0);
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_rules_preserves_small_xattr_natively() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-xattr-native")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        let xattr_file = input_dir.join("--xattr.txt");
        fs::write(&xattr_file, b"xattr payload").unwrap();
        let xattr_name = "user.remanence_test";
        if xattr::set(&xattr_file, xattr_name, b"kept").is_err() {
            return;
        }
        let rules = temp.path().join("empty.rules");
        fs::write(&rules, "").unwrap();
        let out_path = temp.path().join("xattr.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-xattr",
            "--caller-object-id",
            "caller-xattr",
            "--manifest-file-id",
            "manifest-xattr",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        assert_eq!(report["ingest"]["scan"]["totals"]["wrapped_entries"], 0);
        assert_eq!(report["ingest"]["scan"]["totals"]["native_entries"], 1);
        let files = report["files"].as_array().expect("files array");
        let xattr_entry = files
            .iter()
            .find(|file| file["path"] == "--xattr.txt")
            .expect("native xattr file");
        assert_eq!(xattr_entry["xattrs"][xattr_name], bytes_to_hex(b"kept"));

        let restore_dir = temp.path().join("restore");
        let (code, _stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            restore_dir.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        assert_eq!(
            fs::read(restore_dir.join("--xattr.txt")).unwrap(),
            b"xattr payload"
        );
        assert_eq!(
            xattr::get(restore_dir.join("--xattr.txt"), xattr_name).unwrap(),
            Some(b"kept".to_vec())
        );
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_rules_drop_xattrs_without_schema_bump() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-xattr-drop")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        let file = input_dir.join("drop.txt");
        fs::write(&file, b"drop payload").unwrap();
        let xattr_name = "user.remanence_drop";
        if xattr::set(&file, xattr_name, b"noise").is_err() {
            return;
        }
        let rules = temp.path().join("drop.rules");
        fs::write(&rules, format!("xattr-drop {xattr_name}\n")).unwrap();
        let out_path = temp.path().join("drop.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-xattr-drop",
            "--caller-object-id",
            "caller-xattr-drop",
            "--manifest-file-id",
            "manifest-xattr-drop",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        assert_eq!(report["ingest"]["scan"]["totals"]["dropped_xattrs"], 1);
        assert!(report["ingest"]["scan"]["xattr_drops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cluster| cluster["name"] == xattr_name && cluster["reason"] == "policy"));
        let scan = scan_plaintext_rao_entry_locators(&out_path, 4096).unwrap();
        assert_eq!(
            scan.global_pax
                .get("REMANENCE.schema_version")
                .map(String::as_str),
            Some("1.0")
        );
        let files = report["files"].as_array().expect("files array");
        let entry = files
            .iter()
            .find(|file| file["path"] == "drop.txt")
            .expect("native file");
        assert!(entry.get("xattrs").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_rules_allowlist_drops_unlisted_xattr() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-xattr-allowlist")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        let file = input_dir.join("allow.txt");
        fs::write(&file, b"allowlist payload").unwrap();
        if xattr::set(&file, "user.unlisted", b"drop").is_err() {
            return;
        }
        let rules = temp.path().join("allow.rules");
        fs::write(
            &rules,
            "option xattr-mode allowlist\nxattr-keep user.kept\n",
        )
        .unwrap();
        let out_path = temp.path().join("allow.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-xattr-allow",
            "--caller-object-id",
            "caller-xattr-allow",
            "--manifest-file-id",
            "manifest-xattr-allow",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        assert_eq!(report["ingest"]["scan"]["totals"]["dropped_xattrs"], 1);
        assert!(report["ingest"]["scan"]["xattr_drops"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cluster| cluster["name"] == "user.unlisted" && cluster["reason"] == "policy"));
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_rules_wraps_oversized_xattr() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-xattr-large")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        let file = input_dir.join("large-xattr.txt");
        fs::write(&file, b"large xattr payload").unwrap();
        let large_xattr = vec![0x55; 4097];
        if xattr::set(&file, "user.large", &large_xattr).is_err() {
            return;
        }
        let rules = temp.path().join("large.rules");
        fs::write(&rules, "").unwrap();
        let out_path = temp.path().join("large.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-xattr-large",
            "--caller-object-id",
            "caller-xattr-large",
            "--manifest-file-id",
            "manifest-xattr-large",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        assert_eq!(report["ingest"]["scan"]["totals"]["wrapped_entries"], 1);
        assert!(report["ingest"]["scan"]["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cluster| cluster["reason"] == "xattr-large"));
        let files = report["files"].as_array().expect("files array");
        assert!(files
            .iter()
            .any(|file| file["path"] == "large-xattr.txt.remwrap.tar"));
    }

    #[test]
    fn xattr_ruleset_rejects_mismatched_directive_mode() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-xattr-rules")
            .tempdir()
            .unwrap();
        let deny = temp.path().join("deny.rules");
        fs::write(&deny, "xattr-keep user.foo\n").unwrap();
        assert!(archive_ingest::scan_only_report(
            &[temp.path().to_path_buf()],
            Some(&deny),
            false,
            archive_ingest::ScanTuning::default()
        )
        .unwrap_err()
        .contains("xattr-keep requires"));

        let allow = temp.path().join("allow.rules");
        fs::write(&allow, "option xattr-mode allowlist\nxattr-drop user.foo\n").unwrap();
        assert!(archive_ingest::scan_only_report(
            &[temp.path().to_path_buf()],
            Some(&allow),
            false,
            archive_ingest::ScanTuning::default()
        )
        .unwrap_err()
        .contains("xattr-drop requires"));
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_rules_emits_native_hardlinks() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-hardlink")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("links")).unwrap();
        fs::write(input_dir.join("links/a-original.bin"), b"same-inode").unwrap();
        fs::hard_link(
            input_dir.join("links/a-original.bin"),
            input_dir.join("links/b-alias.bin"),
        )
        .unwrap();
        let rules = temp.path().join("empty.rules");
        fs::write(&rules, "").unwrap();
        let out_path = temp.path().join("hardlinks.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-hardlinks",
            "--caller-object-id",
            "caller-hardlinks",
            "--manifest-file-id",
            "manifest-hardlinks",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        assert_eq!(report["ingest"]["scan"]["totals"]["blob_entries"], 0);
        assert_eq!(report["ingest"]["scan"]["totals"]["native_entries"], 2);
        let files = report["files"].as_array().expect("files array");
        assert!(files
            .iter()
            .any(|file| file["path"] == "links/a-original.bin" && file["entry_type"] == "regular"));
        assert!(files.iter().any(|file| file["path"] == "links/b-alias.bin"
            && file["entry_type"] == "hardlink"
            && file["link_target"] == "links/a-original.bin"));

        let restore_dir = temp.path().join("restore");
        let (code, _stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            restore_dir.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let original = fs::metadata(restore_dir.join("links/a-original.bin")).unwrap();
        let alias = fs::metadata(restore_dir.join("links/b-alias.bin")).unwrap();
        assert_eq!(original.ino(), alias.ino());
        assert_eq!(original.nlink(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_rules_hardlink_primary_falls_back_after_exclusion() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-hardlink-excluded-primary")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("links")).unwrap();
        fs::write(input_dir.join("links/a-primary.bin"), b"same-inode").unwrap();
        fs::hard_link(
            input_dir.join("links/a-primary.bin"),
            input_dir.join("links/b-survivor.bin"),
        )
        .unwrap();
        let rules = temp.path().join("exclude.rules");
        fs::write(&rules, "exclude links/a-primary.bin\n").unwrap();
        let out_path = temp.path().join("hardlink-excluded-primary.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-hardlink-excluded",
            "--caller-object-id",
            "caller-hardlink-excluded",
            "--manifest-file-id",
            "manifest-hardlink-excluded",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let report: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        assert_eq!(report["ingest"]["scan"]["totals"]["native_entries"], 1);
        assert_eq!(report["ingest"]["scan"]["totals"]["excluded_entries"], 1);
        let files = report["files"].as_array().expect("files array");
        assert!(files.iter().any(|file| {
            file["path"] == "links/b-survivor.bin" && file["entry_type"] == "regular"
        }));
        assert!(!files.iter().any(|file| file["entry_type"] == "hardlink"));
    }

    #[test]
    fn archive_build_recipient_envelope_reports_inspects_and_extracts() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-build-envelope")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        fs::write(input_dir.join("secret.txt"), b"recipient payload").unwrap();

        let primary =
            remanence_aead::RecipientPrivateKey::new([0x24; 16], "primary", [0x42; 32]).unwrap();
        let recovery =
            remanence_aead::RecipientPrivateKey::new([0x25; 16], "recovery", [0x43; 32]).unwrap();
        let primary_public_path = temp.path().join("primary.raor");
        let recovery_public_path = temp.path().join("recovery.raor");
        let primary_private_path = temp.path().join("primary.raop");
        fs::write(
            &primary_public_path,
            primary.public_key(0).unwrap().serialize().unwrap(),
        )
        .unwrap();
        fs::write(
            &recovery_public_path,
            recovery.public_key(1).unwrap().serialize().unwrap(),
        )
        .unwrap();
        fs::write(&primary_private_path, primary.serialize()).unwrap();
        let out_path = temp.path().join("encrypted.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--recipient",
            primary_public_path.to_str().unwrap(),
            "--recipient",
            recovery_public_path.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-encrypted",
            "--caller-object-id",
            "caller-encrypted",
            "--manifest-file-id",
            "manifest-encrypted",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);

        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty(), "{stderr}");
        let report: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(report["format_version"], 2);
        assert_eq!(
            report["recipient_epochs"],
            json!([
                {
                    "epoch_id": "24242424242424242424242424242424",
                    "label": "primary"
                },
                {
                    "epoch_id": "25252525252525252525252525252525",
                    "label": "recovery"
                }
            ])
        );

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "inspect",
            "--object",
            out_path.to_str().unwrap(),
        ]);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty(), "{stderr}");
        let inspected: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(inspected["format_version"], 2);
        assert_eq!(inspected["recipient_epochs"], report["recipient_epochs"]);

        let restore_dir = temp.path().join("restore");
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            restore_dir.to_str().unwrap(),
            "--private-key",
            primary_private_path.to_str().unwrap(),
        ]);
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(stderr.is_empty(), "{stderr}");
        let extracted: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(extracted["files_written"], 1);
        assert_eq!(
            fs::read(restore_dir.join("secret.txt")).unwrap(),
            b"recipient payload"
        );
    }

    #[test]
    fn archive_build_encrypted_rules_blob_member_uses_ranged_pfr() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-encrypted-blob")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("Blob")).unwrap();
        fs::write(input_dir.join("Blob/member.bin"), b"encrypted blob member").unwrap();
        let rules = temp.path().join("encrypted.rules");
        fs::write(&rules, "blob Blob/\n").unwrap();
        let (_primary, primary_public, recovery_public, primary_private) =
            write_test_recipient_files(temp.path(), "blob", 0x24);
        let out_path = temp.path().join("encrypted-blob.rao");

        let (code, _stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--rules",
            rules.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--recipient",
            primary_public.to_str().unwrap(),
            "--recipient",
            recovery_public.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--object-id",
            "object-encrypted-blob",
            "--caller-object-id",
            "caller-encrypted-blob",
            "--manifest-file-id",
            "manifest-encrypted-blob",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");

        let member_dir = temp.path().join("encrypted-member-restore");
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            member_dir.to_str().unwrap(),
            "--private-key",
            primary_private.to_str().unwrap(),
            "--blob-entry",
            "Blob.remwrap.tar",
            "--blob-member",
            "Blob/member.bin",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let member: serde_json::Value =
            serde_json::from_str(&stdout).expect("encrypted member json");
        assert_eq!(member["mode"], "blob-member");
        assert_eq!(member["range_method"], "rao-entry-range");
        assert_eq!(member["representation"], "encrypted");
        assert_eq!(member["encryption"], "RAO1");
        assert_eq!(member["idx_entry"], "Blob.remwrap.idx");
        assert_eq!(
            member["blob_range_len"],
            b"encrypted blob member".len() as u64
        );
        assert!(member["idx_authenticated_chunks"].as_u64().unwrap() >= 1);
        assert!(member["blob_authenticated_chunks"].as_u64().unwrap() >= 1);
        assert!(member["idx_stored_range_start"].is_number());
        assert!(member["blob_stored_range_start"].is_number());
        assert_eq!(
            fs::read(member_dir.join("Blob/member.bin")).unwrap(),
            b"encrypted blob member"
        );
    }

    #[test]
    fn archive_build_encrypted_rejects_overlong_object_id_before_output() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-build-encrypted-long-object-id")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        fs::write(input_dir.join("secret.txt"), b"classified payload").unwrap();
        let (_primary, primary_public, recovery_public, _primary_private) =
            write_test_recipient_files(temp.path(), "long-id", 0x42);
        let out_path = temp.path().join("encrypted.rao");
        let object_id = "x".repeat(65);
        let argv = vec![
            "rem".to_string(),
            "archive".to_string(),
            "build".to_string(),
            "--inputs".to_string(),
            input_dir.to_str().unwrap().to_string(),
            "--out".to_string(),
            out_path.to_str().unwrap().to_string(),
            "--recipient".to_string(),
            primary_public.to_str().unwrap().to_string(),
            "--recipient".to_string(),
            recovery_public.to_str().unwrap().to_string(),
            "--chunk-size".to_string(),
            "4KiB".to_string(),
            "--object-id".to_string(),
            object_id,
        ];
        let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();

        let (code, stdout, stderr) = invoke_without_discovery(&argv);

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert!(stdout.is_empty(), "{stdout}");
        assert!(
            stderr.contains("--object-id is invalid for encrypted RAO"),
            "{stderr}"
        );
        assert!(
            !out_path.exists(),
            "encrypted build must reject overlong object_id before creating output"
        );
    }

    #[test]
    fn archive_extract_encrypted_range_uses_covering_chunks_only() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-range-encrypted")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        let payload: Vec<u8> = (0..1800).map(|index| (index % 251) as u8).collect();
        fs::write(input_dir.join("big.bin"), &payload).unwrap();
        let (primary, primary_public, recovery_public, primary_private) =
            write_test_recipient_files(temp.path(), "range", 0x52);
        let out_path = temp.path().join("encrypted-range.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--recipient",
            primary_public.to_str().unwrap(),
            "--recipient",
            recovery_public.to_str().unwrap(),
            "--chunk-size",
            "512",
            "--object-id",
            "object-range",
            "--caller-object-id",
            "caller-range",
            "--manifest-file-id",
            "manifest-range",
            "--timestamp",
            "2026-01-01T00:00:00Z",
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let build: serde_json::Value = serde_json::from_str(&stdout).expect("build json");
        let file = &build["files"].as_array().expect("files")[0];
        let first_chunk_lba = file["first_chunk_lba"].as_u64().unwrap();
        let file_size_bytes = file["size_bytes"].as_u64().unwrap();
        assert!(file["chunk_count"].as_u64().unwrap() >= 4);

        let mut stored = fs::read(&out_path).unwrap();
        let inspected = remanence_aead::inspect_bytes(&stored).unwrap();
        let unrequested_chunk = first_chunk_lba + 2;
        let corrupt_offset = remanence_aead::cipher_offset(
            inspected.header.key_frame_len,
            inspected.header.metadata_frame_len,
            512,
            unrequested_chunk,
        )
        .unwrap() as usize;
        stored[corrupt_offset] ^= 0x40;
        fs::write(&out_path, &stored).unwrap();

        let mut full_source = remanence_library::FileBlockSource::open(&out_path, 512).unwrap();
        let full_blocks = full_source.block_count();
        assert!(
            remanence_format::read_encrypted_rao_object(
                &mut full_source,
                512,
                full_blocks,
                &primary
            )
            .is_err(),
            "full encrypted open should fail after unrequested chunk damage"
        );

        let range_dir = temp.path().join("range-restore");
        let first_chunk_lba_text = first_chunk_lba.to_string();
        let file_size_text = file_size_bytes.to_string();
        let argv = vec![
            "rem".to_string(),
            "archive".to_string(),
            "extract".to_string(),
            "--object".to_string(),
            out_path.to_str().unwrap().to_string(),
            "--dest".to_string(),
            range_dir.to_str().unwrap().to_string(),
            "--private-key".to_string(),
            primary_private.to_str().unwrap().to_string(),
            "--path".to_string(),
            "big.bin".to_string(),
            "--first-chunk-lba".to_string(),
            first_chunk_lba_text,
            "--file-size-bytes".to_string(),
            file_size_text,
            "--range".to_string(),
            "400:500".to_string(),
        ];
        let argv = argv.iter().map(String::as_str).collect::<Vec<_>>();
        let (code, stdout, stderr) = invoke_without_discovery(&argv);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let extract: serde_json::Value = serde_json::from_str(&stdout).expect("extract json");
        assert_eq!(extract["mode"], "range");
        assert_eq!(extract["representation"], "encrypted");
        assert_eq!(extract["path"], "big.bin");
        assert_eq!(extract["range_start"], 400);
        assert_eq!(extract["range_len"], 500);
        assert_eq!(extract["bytes_written"], 500);
        assert_eq!(extract["authenticated_chunks"], 2);
        assert_eq!(extract["first_authenticated_chunk"], first_chunk_lba);
        assert_eq!(
            fs::read(range_dir.join("big.bin")).unwrap(),
            payload[400..900]
        );
    }

    #[cfg(feature = "foreign-bru")]
    #[test]
    fn archive_scan_dump_prints_catalog_entries() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-bru-scan")
            .tempdir()
            .unwrap();
        let dump = temp.path().join("fixture.bru");
        let bytes = [
            archive_block().as_slice(),
            file_header_block("camera/a.txt", 3, b"abc").as_slice(),
        ]
        .concat();
        fs::write(&dump, bytes).unwrap();
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "scan",
            "--format",
            "bru",
            "--dump",
            dump.to_str().unwrap(),
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();

        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called for archive dump scan")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("entries: 1"));
        assert!(stdout.contains("damage-events: 0"));
        assert!(stdout.contains("bru:1\tregular\t3\tcamera/a.txt"));
        assert!(err.is_empty());
    }

    #[cfg(feature = "foreign-bru")]
    #[test]
    fn archive_restore_dump_writes_filesystem_output() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-bru-restore")
            .tempdir()
            .unwrap();
        let dump = temp.path().join("fixture.bru");
        let restore = temp.path().join("restore");
        let bytes = [
            archive_block().as_slice(),
            file_header_block("camera/a.txt", 3, b"abc").as_slice(),
        ]
        .concat();
        fs::write(&dump, bytes).unwrap();
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "restore",
            "--format",
            "bru",
            "--dump",
            dump.to_str().unwrap(),
            "--dest",
            restore.to_str().unwrap(),
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();

        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called for archive dump restore")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(fs::read(restore.join("camera/a.txt")).unwrap(), b"abc");
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("files-written: 1"));
        assert!(stdout.contains("bytes-written: 3"));
        assert!(err.is_empty());
    }

    #[cfg(feature = "foreign-bru")]
    #[test]
    fn archive_recover_dump_writes_sparse_output_and_manifest() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-bru-recover")
            .tempdir()
            .unwrap();
        let dump = temp.path().join("fixture.bru");
        let restore = temp.path().join("recover");
        let bytes = [
            archive_block().as_slice(),
            file_header_block("camera/a.txt", 3, b"abc").as_slice(),
        ]
        .concat();
        fs::write(&dump, bytes).unwrap();
        let cli = Cli::parse_from([
            "rem",
            "archive",
            "recover",
            "--format",
            "bru",
            "--dump",
            dump.to_str().unwrap(),
            "--dest",
            restore.to_str().unwrap(),
        ]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();

        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called for archive dump recover")
            },
            &mut out,
            &mut err,
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(fs::read(restore.join("camera/a.txt")).unwrap(), b"abc");
        let manifest = restore.join(".remanence/recovery.jsonl");
        assert!(manifest.exists());
        let manifest_text = fs::read_to_string(manifest).unwrap();
        assert!(manifest_text.contains("\"kind\":\"file\""));
        assert!(manifest_text.contains("\"status\":\"complete\""));
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("files-seen: 1"));
        assert!(stdout.contains("statuses: complete=1 partial=0 missing=0 skipped=0"));
        assert!(stdout.contains("recovery\tbru:1\tcomplete\t3"));
        assert!(err.is_empty());
    }

    #[test]
    fn libraries_command_returns_exit_1_on_discovery_error() {
        let (code, out, err) = invoke(
            &["rem", "libraries"],
            Err(DiscoveryError::NoLibraries { warnings: vec![] }),
        );
        // Can't compare ExitCode directly; round-trip via Debug repr.
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert_eq!(out, "");
        assert!(err.contains("no tape libraries reachable"));
        // No warnings → no warning block, no setcap hint.
        assert!(!err.contains("warnings ("));
        assert!(!err.contains("CAP_SYS_RAWIO"));
    }

    #[test]
    fn no_libraries_error_with_warnings_prints_them_on_stderr() {
        // The host has changers but every probe failed (e.g., the
        // operator forgot setcap, or DVCID came back garbled). We
        // should print every warning so the operator can see *why*
        // discovery returned nothing.
        let warnings = vec![
            DiscoveryWarning::ScsiError {
                path: PathBuf::from("/dev/sg4"),
                command: "INQUIRY VPD 0x80",
                summary: "transport error: I/O failed".into(),
            },
            DiscoveryWarning::DeviceUnreachable {
                path: PathBuf::from("/dev/sg5"),
                source: remanence_library::IoErrorKind {
                    kind: "PermissionDenied",
                    message: "EACCES".into(),
                    raw_os_error: Some(13),
                },
            },
        ];
        let (code, out, err) = invoke(
            &["rem", "libraries"],
            Err(DiscoveryError::NoLibraries { warnings }),
        );
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(1)));
        assert_eq!(out, "");
        assert!(err.contains("no tape libraries reachable"));
        assert!(err.contains("warnings (2):"));
        assert!(err.contains("transport error"));
        assert!(err.contains("/dev/sg5"));
        // Mixed warnings — not all EPERM — so no setcap hint.
        assert!(!err.contains("CAP_SYS_RAWIO"));
    }

    #[test]
    fn no_libraries_with_all_eperm_warnings_prints_setcap_hint() {
        // Every SCSI probe returned EPERM: the signature of missing
        // CAP_SYS_RAWIO. The CLI should nudge toward setcap rather
        // than making the operator strace.
        let warnings = vec![
            DiscoveryWarning::ScsiError {
                path: PathBuf::from("/dev/sg4"),
                command: "READ ELEMENT STATUS",
                summary: "transport error: ioctl SG_IO -1 EPERM".into(),
            },
            DiscoveryWarning::ScsiError {
                path: PathBuf::from("/dev/sg5"),
                command: "READ ELEMENT STATUS",
                summary: "transport error: Operation not permitted".into(),
            },
        ];
        let (_, _out, err) = invoke(
            &["rem", "libraries"],
            Err(DiscoveryError::NoLibraries { warnings }),
        );
        assert!(err.contains("warnings (2):"));
        assert!(err.contains("hint:"));
        assert!(err.contains("CAP_SYS_RAWIO"));
        assert!(err.contains("setcap cap_sys_rawio+ep"));
        assert!(err.contains("INSTALL.md"));
    }

    // -- `rem libraries` ---------------------------------------------

    #[test]
    fn libraries_prints_one_summary_line_per_library() {
        let mut lib = fake_library("LIB001");
        lib.drive_bays = vec![DriveBay {
            element_address: 1,
            accessible: true,
            exception: None,
            installed: None,
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }];
        lib.slots = vec![
            Slot {
                element_address: 1000,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("L00001".into()),
            },
            Slot {
                element_address: 1001,
                accessible: true,
                exception: None,
                full: false,
                cartridge: None,
            },
        ];
        let report = DiscoveryReport {
            libraries: vec![lib],
            warnings: vec![],
        };
        let (code, out, _err) = invoke(&["rem", "libraries"], Ok(report));
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(out.starts_with("LIB001"), "got: {out:?}");
        assert!(out.contains("1 drives"));
        assert!(out.contains("2 slots"));
        assert!(out.contains("1 loaded"));
    }

    #[test]
    fn libraries_json_prints_stable_machine_readable_summary() {
        let mut lib = fake_library("LIBJSON");
        lib.drive_bays = vec![DriveBay {
            element_address: 1,
            accessible: true,
            exception: None,
            installed: None,
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }];
        lib.slots = vec![Slot {
            element_address: 1000,
            accessible: true,
            exception: None,
            full: true,
            cartridge: Some("L00001".into()),
        }];
        let report = DiscoveryReport {
            libraries: vec![lib],
            warnings: vec![],
        };

        let (code, out, err) = invoke(&["rem", "libraries", "--json"], Ok(report));

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(err, "");
        let payload: Value = serde_json::from_str(out.trim()).expect("json");
        let libraries = payload["libraries"].as_array().expect("libraries array");
        assert_eq!(libraries.len(), 1);
        assert_eq!(libraries[0]["serial"], "LIBJSON");
        assert_eq!(libraries[0]["drive_count"], 1);
        assert_eq!(libraries[0]["slot_count"], 1);
        assert_eq!(libraries[0]["loaded_slot_count"], 1);
    }

    #[test]
    fn libs_alias_dispatches_to_libraries() {
        let report = DiscoveryReport {
            libraries: vec![fake_library("LIB_A")],
            warnings: vec![],
        };
        let (code, out, _err) = invoke(&["rem", "libs"], Ok(report));
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(out.contains("LIB_A"));
    }

    // -- `rem library <serial>` --------------------------------------

    #[test]
    fn library_subcommand_shows_focused_view() {
        let mut lib = fake_library("LIBFOCUS");
        lib.drive_bays = vec![
            DriveBay {
                element_address: 1,
                accessible: true,
                exception: None,
                installed: Some(InstalledDrive {
                    serial: "DRIVE_AAA".into(),
                    identity_source: IdentitySource::DvcidInline,
                    vendor: Some("HPE".into()),
                    product: Some("Ultrium 9-SCSI".into()),
                    revision: Some("HH90".into()),
                    sg_path: Some(PathBuf::from("/dev/sg0")),
                    sysfs_path: None,
                }),
                loaded: false,
                loaded_tape: None,
                source_slot: None,
            },
            DriveBay {
                element_address: 2,
                accessible: true,
                exception: None,
                installed: None,
                loaded: false,
                loaded_tape: None,
                source_slot: None,
            },
        ];
        let report = DiscoveryReport {
            libraries: vec![lib],
            warnings: vec![],
        };
        let (code, out, err) = invoke(&["rem", "library", "LIBFOCUS"], Ok(report));
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(out.contains("Library LIBFOCUS"));
        assert!(out.contains("DRIVE_AAA"));
        assert!(out.contains("HPE Ultrium 9-SCSI"));
        assert!(out.contains("/dev/sg0"));
        // Unresolved bay surfaces the "no identity" line.
        assert!(out.contains("[0x0002] (no identity"));
        // Without --slots, no per-slot block.
        assert!(!out.contains("\nSlots:\n"));
        assert_eq!(err, "");
    }

    #[test]
    fn library_subcommand_returns_exit_2_when_serial_not_found() {
        let report = DiscoveryReport {
            libraries: vec![fake_library("LIB_X")],
            warnings: vec![],
        };
        let (code, out, err) = invoke(&["rem", "library", "LIB_NOPE"], Ok(report));
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
        assert_eq!(out, "");
        assert!(err.contains("no library with serial \"LIB_NOPE\""));
        assert!(err.contains("rem libraries"));
    }

    #[test]
    fn library_slots_flag_appends_per_slot_block() {
        let mut lib = fake_library("LIB_SLOTS");
        lib.slots = vec![
            Slot {
                element_address: 0x03e9,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("CLNU01L9".into()),
            },
            Slot {
                element_address: 0x03ea,
                accessible: true,
                exception: None,
                full: true,
                cartridge: Some("S20001L9".into()),
            },
            Slot {
                element_address: 0x03eb,
                accessible: true,
                exception: None,
                full: false,
                cartridge: None,
            },
        ];
        let report = DiscoveryReport {
            libraries: vec![lib],
            warnings: vec![],
        };
        let (code, out, _err) = invoke(&["rem", "library", "LIB_SLOTS", "--slots"], Ok(report));
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(out.contains("Slots:\n"));
        assert!(out.contains("[0x03e9] full   CLNU01L9   (cleaning)"));
        assert!(out.contains("[0x03ea] full   S20001L9"));
        assert!(!out.contains("[0x03ea] full   S20001L9   (cleaning)"));
        assert!(out.contains("[0x03eb] empty"));
    }

    #[test]
    fn library_json_exposes_element_exception_evidence() {
        let mut lib = fake_library("LIB_EXCEPTION_JSON");
        lib.drive_bays = vec![
            DriveBay {
                element_address: 1,
                accessible: false,
                exception: Some(ElementException {
                    asc: 0x04,
                    ascq: 0x01,
                }),
                installed: Some(InstalledDrive {
                    serial: "DRIVE_AAA".into(),
                    identity_source: IdentitySource::DvcidInline,
                    vendor: Some("HPE".into()),
                    product: Some("Ultrium 9-SCSI".into()),
                    revision: Some("HH90".into()),
                    sg_path: Some(PathBuf::from("/dev/sg0")),
                    sysfs_path: None,
                }),
                loaded: true,
                loaded_tape: Some("AOX030L9".into()),
                source_slot: Some(0x03eb),
            },
            DriveBay {
                element_address: 2,
                accessible: true,
                exception: None,
                installed: None,
                loaded: false,
                loaded_tape: None,
                source_slot: None,
            },
        ];
        lib.slots = vec![
            Slot {
                element_address: 0x03e9,
                accessible: false,
                exception: Some(ElementException {
                    asc: 0x3b,
                    ascq: 0x12,
                }),
                full: true,
                cartridge: Some("AOX031L9".into()),
            },
            Slot {
                element_address: 0x03ea,
                accessible: true,
                exception: None,
                full: false,
                cartridge: None,
            },
        ];
        lib.ie_ports = vec![IePort {
            element_address: 0x0010,
            accessible: false,
            exception: Some(ElementException {
                asc: 0x00,
                ascq: 0x00,
            }),
            full: false,
            cartridge: None,
            import_enabled: true,
            export_enabled: true,
        }];
        let report = DiscoveryReport {
            libraries: vec![lib],
            warnings: vec![],
        };

        let (code, out, err) = invoke(
            &["rem", "library", "LIB_EXCEPTION_JSON", "--json", "--slots"],
            Ok(report),
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(err, "");
        let payload: Value = serde_json::from_str(out.trim()).expect("json");
        assert_eq!(payload["drives"][0]["exception"]["asc"], "0x04");
        assert_eq!(payload["drives"][0]["exception"]["asc_raw"], 0x04);
        assert_eq!(payload["drives"][0]["exception"]["ascq"], "0x01");
        assert_eq!(payload["drives"][0]["exception"]["ascq_raw"], 0x01);
        assert!(payload["drives"][1]["exception"].is_null());
        assert_eq!(payload["slots"][0]["exception"]["asc"], "0x3b");
        assert_eq!(payload["slots"][0]["exception"]["asc_raw"], 0x3b);
        assert_eq!(payload["slots"][0]["exception"]["ascq"], "0x12");
        assert_eq!(payload["slots"][0]["exception"]["ascq_raw"], 0x12);
        assert!(payload["slots"][1]["exception"].is_null());
        assert_eq!(payload["ie_ports"][0]["exception"]["asc"], "0x00");
        assert_eq!(payload["ie_ports"][0]["exception"]["asc_raw"], 0x00);
        assert_eq!(payload["ie_ports"][0]["exception"]["ascq"], "0x00");
        assert_eq!(payload["ie_ports"][0]["exception"]["ascq_raw"], 0x00);
    }

    #[test]
    fn library_human_output_prints_element_exception_evidence() {
        let mut lib = fake_library("LIB_EXCEPTION_TEXT");
        lib.drive_bays = vec![
            DriveBay {
                element_address: 1,
                accessible: false,
                exception: Some(ElementException {
                    asc: 0x04,
                    ascq: 0x01,
                }),
                installed: Some(InstalledDrive {
                    serial: "DRIVE_AAA".into(),
                    identity_source: IdentitySource::DvcidInline,
                    vendor: Some("HPE".into()),
                    product: Some("Ultrium 9-SCSI".into()),
                    revision: Some("HH90".into()),
                    sg_path: Some(PathBuf::from("/dev/sg0")),
                    sysfs_path: None,
                }),
                loaded: true,
                loaded_tape: Some("AOX030L9".into()),
                source_slot: Some(0x03eb),
            },
            DriveBay {
                element_address: 2,
                accessible: false,
                exception: Some(ElementException {
                    asc: 0x3b,
                    ascq: 0x12,
                }),
                installed: None,
                loaded: false,
                loaded_tape: None,
                source_slot: None,
            },
        ];
        lib.slots = vec![
            Slot {
                element_address: 0x03e9,
                accessible: false,
                exception: Some(ElementException {
                    asc: 0x28,
                    ascq: 0x00,
                }),
                full: true,
                cartridge: Some("AOX031L9".into()),
            },
            Slot {
                element_address: 0x03ea,
                accessible: false,
                exception: Some(ElementException {
                    asc: 0x00,
                    ascq: 0x00,
                }),
                full: false,
                cartridge: None,
            },
        ];
        lib.ie_ports = vec![IePort {
            element_address: 0x0010,
            accessible: false,
            exception: Some(ElementException {
                asc: 0x04,
                ascq: 0x07,
            }),
            full: false,
            cartridge: None,
            import_enabled: true,
            export_enabled: true,
        }];
        let report = DiscoveryReport {
            libraries: vec![lib],
            warnings: vec![],
        };

        let (code, out, err) = invoke(
            &["rem", "library", "LIB_EXCEPTION_TEXT", "--slots"],
            Ok(report),
        );

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(err, "");
        assert!(out.contains(
            "[0x0001] HPE Ultrium 9-SCSI (HH90)  /dev/sg0  serial DRIVE_AAA   exception ASC/ASCQ=0x04/0x01"
        ));
        assert!(
            out.contains("[0x0002] (no identity — see warnings)   exception ASC/ASCQ=0x3b/0x12")
        );
        assert!(out.contains("[0x03e9] full   AOX031L9   exception ASC/ASCQ=0x28/0x00"));
        assert!(out.contains("[0x03ea] empty   exception ASC/ASCQ=0x00/0x00"));
        assert!(
            out.contains("[0x0010] empty   (import:in export:out)   exception ASC/ASCQ=0x04/0x07")
        );
    }

    #[test]
    fn lib_alias_dispatches_to_library() {
        let report = DiscoveryReport {
            libraries: vec![fake_library("LIB_ALIAS")],
            warnings: vec![],
        };
        let (code, out, _err) = invoke(&["rem", "lib", "LIB_ALIAS"], Ok(report));
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(out.contains("Library LIB_ALIAS"));
    }

    // -- Warning routing ---------------------------------------------

    #[test]
    fn warnings_print_to_stderr_after_success() {
        let lib = fake_library("LIB_W");
        let report = DiscoveryReport {
            libraries: vec![lib],
            warnings: vec![
                DiscoveryWarning::DriveMappingUnavailable {
                    library: "LIB_W".into(),
                },
                DiscoveryWarning::UnclaimedTape {
                    sg_path: PathBuf::from("/dev/sg9"),
                    serial: "ORPHAN1".into(),
                },
            ],
        };
        let (code, out, err) = invoke(&["rem", "libraries"], Ok(report));
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        // Output is clean of warning chatter.
        assert!(!out.contains("warning"));
        // Warnings show on stderr with the count and each variant.
        assert!(err.contains("warnings (2):"));
        assert!(err.contains("library LIB_W: drive identity unavailable"));
        assert!(err.contains("/dev/sg9"));
        assert!(err.contains("ORPHAN1"));
    }

    // -- Dirty-snapshot recovery formatter ---------------------------

    #[test]
    fn dirty_recovery_hint_partial_failure_wording() {
        let mut buf = Vec::<u8>::new();
        print_dirty_snapshot_recovery("LIB_X", DirtyReason::PartialFailure, &mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert!(
            s.contains("partially succeeded"),
            "partial-failure wording missing: {s}"
        );
        assert!(s.contains("rem library LIB_X --slots"));
        // Recovery commands must include --allow so they actually
        // pass the pre-discovery gate.
        assert!(s.contains("rem-debug rescan  LIB_X --allow LIB_X"));
    }

    #[test]
    fn dirty_recovery_hint_completion_unknown_wording() {
        let mut buf = Vec::<u8>::new();
        print_dirty_snapshot_recovery("LIB_R", DirtyReason::CompletionUnknown, &mut buf);
        let s = String::from_utf8(buf).unwrap();
        // Completion-unknown wording: NOT "partially succeeded"
        // (single-CDB failures shouldn't claim phase staging), NOT
        // "touched an IE port" (vendor-semantics is its own thing).
        assert!(
            !s.contains("partially succeeded"),
            "completion-unknown leaked partial-failure wording: {s}"
        );
        assert!(
            !s.contains("touched an IE port"),
            "completion-unknown leaked vendor-semantics wording: {s}"
        );
        assert!(
            s.contains("transport-level error") || s.contains("transport-level"),
            "completion-unknown should name transport-level error: {s}"
        );
        assert!(
            s.contains("may have actually executed"),
            "completion-unknown should explain the ambiguity: {s}"
        );
        assert!(s.contains("rem library LIB_R --slots"));
        assert!(s.contains("rem-debug rescan  LIB_R --allow LIB_R"));
    }

    #[test]
    fn dirty_reason_from_dirty_cause_covers_all_variants() {
        // Lock the mapping so a new DirtyCause variant doesn't get
        // silently dropped on the CLI side. Match-on the result so
        // adding a variant trips a `non_exhaustive_patterns` build
        // error (this enum is local; matches are exhaustive here).
        match DirtyReason::from(DirtyCause::PartialFailure) {
            DirtyReason::PartialFailure => {}
            other => panic!("unexpected mapping: {other:?}"),
        }
        match DirtyReason::from(DirtyCause::VendorSemantics) {
            DirtyReason::VendorSemantics => {}
            other => panic!("unexpected mapping: {other:?}"),
        }
        match DirtyReason::from(DirtyCause::CompletionUnknown) {
            DirtyReason::CompletionUnknown => {}
            other => panic!("unexpected mapping: {other:?}"),
        }
    }

    #[test]
    fn dirty_recovery_hint_vendor_semantics_wording() {
        let mut buf = Vec::<u8>::new();
        print_dirty_snapshot_recovery("LIB_Q", DirtyReason::VendorSemantics, &mut buf);
        let s = String::from_utf8(buf).unwrap();
        // The success-path wording must NOT claim the op failed.
        assert!(
            !s.contains("partially succeeded"),
            "vendor-semantics path leaked partial-failure wording: {s}"
        );
        assert!(
            s.contains("touched an IE port"),
            "vendor-semantics wording missing: {s}"
        );
        assert!(s.contains("vault"));
        assert!(s.contains("rem library LIB_Q --slots"));
        assert!(s.contains("rem-debug rescan  LIB_Q --allow LIB_Q"));
    }

    #[test]
    fn warnings_print_to_stderr_even_when_named_library_missing() {
        // exit 2 path still emits the warnings — operator sees them
        // regardless of whether the focused command found its target.
        let report = DiscoveryReport {
            libraries: vec![fake_library("LIB_KNOWN")],
            warnings: vec![DiscoveryWarning::UnclaimedTape {
                sg_path: PathBuf::from("/dev/sg9"),
                serial: "FREE".into(),
            }],
        };
        let (code, _out, err) = invoke(&["rem", "library", "MISSING"], Ok(report));
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::from(2)));
        assert!(err.contains("no library with serial \"MISSING\""));
        assert!(err.contains("warnings (1):"));
        assert!(err.contains("FREE"));
    }

    const BRU_BLOCK_SIZE: usize = 2048;
    const CHKSUM_OFFSET: usize = 0x080;
    const CHKSUM_SIZE: usize = 8;
    const CHKSUM_PLACEHOLDER: &[u8; CHKSUM_SIZE] = b"       0";
    const MAGIC_OFFSET: usize = 0x0B0;
    const MAGIC_SIZE: usize = 4;
    const MAGIC_ARCHIVE_HEADER: u64 = 0x1234;
    #[cfg(feature = "foreign-bru")]
    const MAGIC_FILE_HEADER: u64 = 0x2345;
    const ARTIME_OFFSET: usize = 0x098;
    const BUFSIZE_OFFSET: usize = 0x0A0;
    const RELEASE_MINOR_OFFSET: usize = 0x0B8;
    const RELEASE_MAJOR_OFFSET: usize = 0x0BC;
    const VARIANT_OFFSET: usize = 0x0C0;
    const ARCHIVE_ID_LOW_OFFSET: usize = 0x0D8;
    const LABEL_OFFSET: usize = 0x1D0;
    #[cfg(feature = "foreign-bru")]
    const FILE_PATH_OFFSET: usize = 0x000;
    #[cfg(feature = "foreign-bru")]
    const INLINE_DATA_LEN_OFFSET: usize = 0x0DC;
    #[cfg(feature = "foreign-bru")]
    const INLINE_DATA_OFFSET: usize = 0x400;
    #[cfg(feature = "foreign-bru")]
    const ST_MODE_OFFSET: usize = 0x180;
    #[cfg(feature = "foreign-bru")]
    const ST_SIZE_OFFSET: usize = 0x1B8;
    #[cfg(feature = "foreign-bru")]
    const S_IFREG: u64 = 0x8000;

    fn put_ascii(block: &mut [u8; BRU_BLOCK_SIZE], offset: usize, text: &str) {
        block[offset..offset + text.len()].copy_from_slice(text.as_bytes());
    }

    fn put_hex(block: &mut [u8; BRU_BLOCK_SIZE], offset: usize, size: usize, value: u64) {
        put_ascii(block, offset, &format!("{value:0size$x}"));
    }

    fn bru_checksum(block: &[u8; BRU_BLOCK_SIZE]) -> u32 {
        let mut sums = [0u32; 4];
        for (index, byte) in block.iter().enumerate() {
            let value = if (CHKSUM_OFFSET..CHKSUM_OFFSET + CHKSUM_SIZE).contains(&index) {
                CHKSUM_PLACEHOLDER[index - CHKSUM_OFFSET]
            } else {
                *byte
            };
            sums[index % 4] = sums[index % 4].wrapping_add(value as u32);
        }
        ((sums[0] & 0xff) << 24)
            | ((sums[1] & 0xff) << 16)
            | ((sums[2] & 0xff) << 8)
            | (sums[3] & 0xff)
    }

    fn finalize_block(mut block: [u8; BRU_BLOCK_SIZE]) -> [u8; BRU_BLOCK_SIZE] {
        let checksum = bru_checksum(&block);
        put_ascii(&mut block, CHKSUM_OFFSET, &format!("{checksum:08x}"));
        block
    }

    fn archive_block() -> [u8; BRU_BLOCK_SIZE] {
        let mut block = [0; BRU_BLOCK_SIZE];
        put_hex(&mut block, MAGIC_OFFSET, MAGIC_SIZE, MAGIC_ARCHIVE_HEADER);
        put_hex(&mut block, ARTIME_OFFSET, 8, 0x4DE47D26);
        put_hex(&mut block, BUFSIZE_OFFSET, 8, 1024 * 1024);
        put_hex(&mut block, RELEASE_MINOR_OFFSET, 4, 17);
        put_hex(&mut block, RELEASE_MAJOR_OFFSET, 4, 1);
        put_hex(&mut block, VARIANT_OFFSET, 4, 0);
        put_hex(&mut block, ARCHIVE_ID_LOW_OFFSET, 4, 0x61A8);
        put_ascii(&mut block, LABEL_OFFSET, "TEST");
        finalize_block(block)
    }

    #[cfg(feature = "foreign-bru")]
    fn file_header_block(path: &str, size: u64, inline: &[u8]) -> [u8; BRU_BLOCK_SIZE] {
        let mut block = [0; BRU_BLOCK_SIZE];
        put_ascii(&mut block, FILE_PATH_OFFSET, path);
        put_hex(&mut block, MAGIC_OFFSET, MAGIC_SIZE, MAGIC_FILE_HEADER);
        put_hex(&mut block, INLINE_DATA_LEN_OFFSET, 4, inline.len() as u64);
        put_hex(&mut block, ST_MODE_OFFSET, 8, S_IFREG | 0o644);
        put_hex(&mut block, ST_SIZE_OFFSET, 8, size);
        block[INLINE_DATA_OFFSET..INLINE_DATA_OFFSET + inline.len()].copy_from_slice(inline);
        finalize_block(block)
    }
}
