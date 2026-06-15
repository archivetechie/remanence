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

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Cursor, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use remanence_aead::{header::object_id_field, inspect_bytes, open_to_vec, RootKey};
use remanence_api::pb;
use remanence_bru::{BruFormat, BRU_BLOCK_SIZE};
#[cfg(target_os = "linux")]
use remanence_format::ForeignTapeFormat;
use remanence_format::{
    read_encrypted_rao_file_range_to_vec, read_rem_tar_object,
    write_encrypted_rao_object_from_readers, write_rem_tar_object_from_readers, ArchiveGapCause,
    ArchiveGapRange, ArchiveReader, BodyLba, DamageRange, DamageStatus, EntryKind,
    FormatDescriptor, FormatError, ProbeConfidence, ProbeResult, RemTarEntryType, RemTarFileLayout,
    RemTarFileSpec, RemTarFileStream, RemTarObjectLayout, RemTarObjectOptions, RemTarReadObject,
    SourceRequirement, FORMAT_ID, MANIFEST_PATH,
};
#[cfg(target_os = "linux")]
use remanence_library::DriveHandlePhysicalSource;
use remanence_library::{
    BlockSize, DirtyCause, DiscoveryError, DiscoveryReport, DiscoveryWarning, DriveBay,
    DriveHandleSink, DriveHandleSource, FileBlockSink, FileBlockSource, IePort, Library, Slot,
    StaticAllowlist, TapeConfig, VecBlockSource, WormMediaState,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tonic::transport::Channel;
use uuid::Uuid;
use zeroize::Zeroize;

mod archive_ingest;
mod pool_ops;

const DEFAULT_DAEMON_ENDPOINT: &str = "unix:/var/lib/rem/rem.sock";
const DEFAULT_DEV_TAPE_RECORD_BYTES: usize = 1024 * 1024;
const MAX_SCSI_VARIABLE_WRITE_BYTES: usize = 0x00FF_FFFF;

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
        | Command::Tape { .. } => None,
    }
}

fn rem_only_reason(cmd: &Command) -> Option<&'static str> {
    match cmd {
        Command::DaemonClient { .. }
        | Command::OperationClient { .. }
        | Command::CatalogClient { .. } => Some("daemon client commands"),
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
            RemCommand::Library { serial, slots } => Self::Library { serial, slots },
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
            RemCommand::Tape { command } => Self::Tape {
                command: command.into(),
            },
            RemCommand::Archive { command } => Self::Archive {
                command: command.into(),
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
        command: ArchiveCommand,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum CatalogUnitOriginFilterArg {
    All,
    Native,
    Foreign,
}

#[derive(Subcommand, Debug)]
enum RemTapeCommand {
    /// Initialize one tape or a slot range.
    Init(TapeInitArgs),

    /// Permanently retire one tape identity in the local catalog.
    Retire(TapeRetireArgs),
}

impl From<RemTapeCommand> for TapeCommand {
    fn from(value: RemTapeCommand) -> Self {
        match value {
            RemTapeCommand::Init(args) => Self::Init(args),
            RemTapeCommand::Retire(args) => Self::Retire(args),
        }
    }
}

#[derive(Subcommand, Debug)]
enum TapeCommand {
    /// Initialize one tape or a slot range.
    Init(TapeInitArgs),

    /// Permanently retire one tape identity in the local catalog.
    Retire(TapeRetireArgs),
}

impl TapeCommand {
    fn validate_before_discovery(&self) -> Result<(), String> {
        match self {
            Self::Init(args) => args.validate_before_discovery(),
            Self::Retire(args) => args.validate_before_discovery(),
        }
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
    /// Build a portable RAO object file from local inputs.
    Build(RemArchiveBuildArgs),

    /// Inspect a portable RAO object file.
    Inspect(RemArchiveInspectArgs),

    /// Extract a portable RAO object file into a directory.
    Extract(RemArchiveExtractArgs),

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

    /// Compatibility parser for direct local archive verification.
    #[command(hide = true)]
    Verify(RemArchiveVerifyArgs),

    /// List native objects from the local catalog (no tape access).
    List(RemArchiveListArgs),
}

/// Arguments for `rem archive build`.
#[derive(Args, Debug)]
struct RemArchiveBuildArgs {
    /// Input files or directories. Directories are expanded recursively.
    #[arg(long = "inputs", value_name = "PATH", num_args = 1.., required = true)]
    inputs: Vec<PathBuf>,

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

    /// Build an encrypted RAO1 object instead of plaintext rao-v1.
    #[arg(long)]
    encrypt: bool,

    /// 32-byte root key file for --encrypt.
    #[arg(long, value_name = "PATH", requires = "encrypt")]
    key_file: Option<PathBuf>,

    /// 16-byte key identifier as 32 lowercase or uppercase hex characters.
    #[arg(long, value_name = "HEX", requires = "encrypt")]
    key_id: Option<String>,

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

    /// 32-byte root key file. Required to inspect encrypted object contents.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,
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

    /// 32-byte root key file. Required for encrypted objects.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,

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

    /// Store the RAO1 encrypted representation instead of plaintext rao-v1.
    #[arg(long)]
    encrypt: bool,

    /// 32-byte root key file for --encrypt.
    #[arg(long, value_name = "PATH", requires = "encrypt")]
    key_file: Option<PathBuf>,

    /// 16-byte key identifier as 32 lowercase or uppercase hex characters.
    #[arg(long, value_name = "HEX", requires = "encrypt")]
    key_id: Option<String>,

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

    /// 32-byte root key file. Required for encrypted object copies.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,

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
    /// Library serial for the physical tape library.
    #[arg(long, value_name = "SERIAL")]
    library: String,

    /// Canonical locator JSON emitted by `archive write --json`.
    #[arg(long, value_name = "JSON")]
    locator: String,

    /// Expected payload SHA-256 (hex) to compare the tape bytes against.
    #[arg(long, value_name = "HEX")]
    expected_sha256: String,

    /// 32-byte root key file. Required for encrypted object copies.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,

    /// Path to `/etc/rem/config.toml`.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,
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

impl From<RemArchiveCommand> for ArchiveCommand {
    fn from(value: RemArchiveCommand) -> Self {
        match value {
            RemArchiveCommand::Build(args) => Self::Build(args.into()),
            RemArchiveCommand::Inspect(args) => Self::Inspect(args.into()),
            RemArchiveCommand::Extract(args) => Self::Extract(args.into()),
            RemArchiveCommand::Probe(args) => Self::Probe(args.into()),
            RemArchiveCommand::Scan(args) => Self::Scan(args.into()),
            RemArchiveCommand::Restore(args) => Self::Restore(args.into()),
            RemArchiveCommand::Recover(args) => Self::Recover(args.into()),
            RemArchiveCommand::Write(args) => Self::Write(args.into()),
            RemArchiveCommand::Read(args) => Self::Read(args.into()),
            RemArchiveCommand::ExportObject(args) => Self::ExportObject(args.into()),
            RemArchiveCommand::Verify(args) => Self::Verify(args.into()),
            RemArchiveCommand::List(args) => Self::List(args.into()),
        }
    }
}

impl From<RemArchiveBuildArgs> for ArchiveBuildArgs {
    fn from(value: RemArchiveBuildArgs) -> Self {
        Self {
            inputs: value.inputs,
            out: value.out,
            rules: value.rules,
            scan_only: value.scan_only,
            manifest_out: value.manifest_out,
            no_index: value.no_index,
            blob_suggest_ratio: value.blob_suggest_ratio,
            blob_suggest_count: value.blob_suggest_count,
            sanity_ceiling_count: value.sanity_ceiling_count,
            encrypt: value.encrypt,
            key_file: value.key_file,
            key_id: value.key_id,
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
            key_file: value.key_file,
        }
    }
}

impl From<RemArchiveExtractArgs> for ArchiveExtractArgs {
    fn from(value: RemArchiveExtractArgs) -> Self {
        Self {
            object: value.object,
            dest: value.dest,
            chunk_size: value.chunk_size,
            key_file: value.key_file,
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

impl From<RemArchiveWriteArgs> for ArchiveWriteArgs {
    fn from(value: RemArchiveWriteArgs) -> Self {
        Self {
            library: value.library,
            file: value.file,
            pool_id: value.pool,
            archive_path: value.archive_path,
            caller_object_id: value.caller_object_id,
            encrypt: value.encrypt,
            key_file: value.key_file,
            key_id: value.key_id,
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
            key_file: value.key_file,
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

impl From<RemArchiveVerifyArgs> for ArchiveVerifyArgs {
    fn from(value: RemArchiveVerifyArgs) -> Self {
        Self {
            library: value.library,
            locator: value.locator,
            expected_sha256: value.expected_sha256,
            key_file: value.key_file,
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
    /// Build a portable RAO object file from local inputs.
    Build(ArchiveBuildArgs),

    /// Inspect a portable RAO object file.
    Inspect(ArchiveInspectArgs),

    /// Extract a portable RAO object file into a directory.
    Extract(ArchiveExtractArgs),

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

/// Arguments for the shared `archive build` command.
#[derive(Args, Debug)]
struct ArchiveBuildArgs {
    /// Input files or directories. Directories are expanded recursively.
    #[arg(long = "inputs", value_name = "PATH", num_args = 1.., required = true)]
    inputs: Vec<PathBuf>,

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

    /// Build an encrypted RAO1 object instead of plaintext rao-v1.
    #[arg(long)]
    encrypt: bool,

    /// 32-byte root key file for --encrypt.
    #[arg(long, value_name = "PATH", requires = "encrypt")]
    key_file: Option<PathBuf>,

    /// 16-byte key identifier as 32 lowercase or uppercase hex characters.
    #[arg(long, value_name = "HEX", requires = "encrypt")]
    key_id: Option<String>,

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

    /// 32-byte root key file. Required to inspect encrypted object contents.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,
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

    /// 32-byte root key file. Required for encrypted objects.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,

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

    /// Store the RAO1 encrypted representation instead of plaintext rao-v1.
    #[arg(long)]
    encrypt: bool,

    /// 32-byte root key file for --encrypt.
    #[arg(long, value_name = "PATH", requires = "encrypt")]
    key_file: Option<PathBuf>,

    /// 16-byte key identifier as 32 lowercase or uppercase hex characters.
    #[arg(long, value_name = "HEX", requires = "encrypt")]
    key_id: Option<String>,

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

    /// 32-byte root key file. Required for encrypted object copies.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,

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

    /// 32-byte root key file. Required for encrypted object copies.
    #[arg(long, value_name = "PATH")]
    key_file: Option<PathBuf>,

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
            Self::Build(_) | Self::Inspect(_) | Self::Extract(_) => None,
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
            | Self::Inspect(_)
            | Self::Extract(_)
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
            Self::Inspect(_) => panic!("ArchiveCommand::Inspect has no dump/tape source"),
            Self::Extract(_) => panic!("ArchiveCommand::Extract has no dump/tape source"),
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
            Self::Inspect(_) => panic!("ArchiveCommand::Inspect has no format"),
            Self::Extract(_) => panic!("ArchiveCommand::Extract has no format"),
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
            Self::Bru => BruFormat.id(),
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

fn run_cli(cli: ParsedCli, mode: CliMode) -> ExitCode {
    #[cfg(target_os = "linux")]
    let discover_fn = remanence_library::discover;
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
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    run_with_mode(cli, mode, discover_fn, &mut out, &mut err)
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
    run_with_mode(cli.into(), CliMode::Rem, discover_fn, out, err)
}

#[cfg(test)]
fn run_debug<F>(cli: DebugCli, discover_fn: F, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode
where
    F: FnOnce() -> Result<DiscoveryReport, DiscoveryError>,
{
    run_with_mode(cli.into(), CliMode::Debug, discover_fn, out, err)
}

fn run_with_mode<F>(
    cli: ParsedCli,
    mode: CliMode,
    discover_fn: F,
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
        // `rem tape retire` is catalog + audit only — no SCSI, no library
        // allowlist — so it bypasses discovery entirely (like the catalog
        // maintenance commands above).
        if let TapeCommand::Retire(args) = command {
            return run_tape_retire(args, out, err);
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
        if let ArchiveCommand::Build(args) = command {
            return run_archive_build(args, out, err);
        }
        if let ArchiveCommand::Inspect(args) = command {
            return run_archive_inspect(args, out, err);
        }
        if let ArchiveCommand::Extract(args) = command {
            return run_archive_extract(args, out, err);
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
        Command::Library { serial, slots } => match report.library(&serial) {
            Some(lib) => {
                print_library(lib, &report, out);
                if slots {
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
        | Command::CatalogClient { .. } => {
            unreachable!("daemon client command dispatched pre-discovery")
        }
        Command::Tape { command } => {
            return run_tape_command(&report, &command, out, err);
        }
        Command::Archive { command } => {
            if command.is_pool_write_command() {
                if let ArchiveCommand::Write(args) = &command {
                    return pool_ops::run_archive_write(
                        &report,
                        &pool_ops::ArchiveWriteArgs {
                            library: args.library.clone(),
                            file: args.file.clone(),
                            pool_id: args.pool_id.clone(),
                            archive_path: args.archive_path.clone(),
                            caller_object_id: args.caller_object_id.clone(),
                            encrypt: args.encrypt,
                            key_file: args.key_file.clone(),
                            key_id: args.key_id.clone(),
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
            if let ArchiveCommand::Read(args) = &command {
                return pool_ops::run_archive_read(
                    &report,
                    &pool_ops::ArchiveReadArgs {
                        library: args.library.clone(),
                        locator: args.locator.clone(),
                        out: args.out.clone(),
                        key_file: args.key_file.clone(),
                        config: args.config.clone(),
                    },
                    &allow,
                    &allow_derived,
                    out,
                    err,
                );
            }
            if let ArchiveCommand::ExportObject(args) = &command {
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
            if let ArchiveCommand::Verify(args) = &command {
                return pool_ops::run_archive_verify(
                    &report,
                    &pool_ops::ArchiveVerifyArgs {
                        library: args.library.clone(),
                        locator: args.locator.clone(),
                        expected_sha256: args.expected_sha256.clone(),
                        key_file: args.key_file.clone(),
                        config: args.config.clone(),
                    },
                    &allow,
                    &allow_derived,
                    out,
                    err,
                );
            }
            return run_archive_tape_command(&report, &command, &allow, &allow_derived, out, err);
        }
        Command::Dev { command } => {
            return run_dev_command(&report, &command, &allow, &allow_derived, out, err);
        }
    }
    print_warnings(&report, err);
    ExitCode::SUCCESS
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
                        .map_err(status_error)?
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
                        .map_err(status_error)?
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
                        })
                        .await
                        .map_err(status_error)?
                        .into_inner()
                        .tapes;
                    print_tape_list(tapes, json_output, out).map_err(DaemonClientError::from)
                }
                CatalogClientCommand::Tape { tape_uuid } => {
                    let tape_uuid = parse_uuid_bytes(tape_uuid, "tape_uuid")
                        .map_err(DaemonClientError::from)?;
                    let tape = client
                        .get_tape(pb::GetTapeRequest { tape_uuid })
                        .await
                        .map_err(status_error)?
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
                        .map_err(status_error)?
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
                        .map_err(status_error)?
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
                        .map_err(status_error)?
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
                        .map_err(status_error)?
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
                        .map_err(status_error)?
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
                        .map_err(status_error)?
                        .into_inner();
                    print_catalog_entry_list(response.entries, json_output, out)
                        .map_err(DaemonClientError::from)
                }
            }
        })
    });
    finish_daemon_client_result(result, json_output, err)
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

fn status_error(error: tonic::Status) -> DaemonClientError {
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
    })
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
            "error: catalog reset requires --i-understand-this-erases-the-catalog"
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
}

fn run_tape_command(
    report: &DiscoveryReport,
    command: &TapeCommand,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    match command {
        TapeCommand::Init(args) => run_tape_init(report, args, out, err),
        TapeCommand::Retire(_) => unreachable!("tape retire dispatched pre-discovery"),
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
    let mut policy = StaticAllowlist::new(config.libraries.iter().map(|lib| lib.serial.clone()));
    for library in config
        .libraries
        .iter()
        .filter(|library| library.allow_derived_drive_identity)
    {
        policy = policy.with_derived_allowed(library.serial.clone());
    }

    if args.dry_run {
        let mut catalog = match remanence_state::CatalogIndex::open_read_only(&paths.sqlite_path) {
            Ok(catalog) => catalog,
            Err(error) => {
                let _ = writeln!(err, "error: {error}");
                print_warnings(report, err);
                return ExitCode::from(1);
            }
        };
        let ctx = TapeInitRunContext {
            report,
            config: &config,
            policy: &policy,
            args,
        };
        return run_tape_init_candidates(&mut catalog, candidates, &ctx, out, err);
    }

    let mut state = match remanence_state::StateHandle::open_with_config(paths, config.clone()) {
        Ok(state) => state,
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            print_warnings(report, err);
            return ExitCode::from(1);
        }
    };
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
    let count = candidates.len();
    for candidate in candidates {
        match run_one_tape_init(state, ctx, candidate.clone(), err) {
            Ok(result) => {
                print_tape_init_result(&result, out);
                if result.success {
                    successes += 1;
                } else {
                    failures += 1;
                }
            }
            Err(error) => {
                let _ = writeln!(err, "tape init {}: {error}", candidate.label());
                failures += 1;
            }
        }
    }
    if count > 1 {
        let _ = writeln!(out, "summary: {successes} ok, {failures} failed");
    }
    print_warnings(ctx.report, err);
    if failures == 0 || ctx.args.dry_run {
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
    let mut handle = library
        .open(policy)
        .map_err(|error| format!("opening library: {error}"))?;
    if candidate.location == TapeInitLocation::Slot {
        handle
            .load(candidate.element_address, drive_element, policy)
            .map_err(|error| format!("load slot 0x{:04x}: {error}", candidate.element_address))?;
    }
    let mut drive = handle
        .open_drive(drive_element, policy)
        .map_err(|error| format!("open drive 0x{drive_element:04x}: {error}"))?;
    let drive_config = drive
        .read_config()
        .map_err(|error| format!("read drive config: {error}"))?;
    if drive_config.write_protected {
        return Err("media is write-protected according to MODE SENSE".to_string());
    }

    drive
        .rewind()
        .map_err(|error| format!("rewind before BOT read: {error}"))?;
    let bot_projection = {
        let mut source = DriveHandleSource(&mut drive);
        remanence_api::classify_bot_from_source(&mut source)
    };
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
        confirm_clobber_data(voltag.as_str(), &decision, err)?
    } else {
        false
    };
    let (planned_uuid, block_size, parity) = planned_init_geometry(
        &bot_projection.classification,
        &decision,
        clobber_data_confirmed,
        fresh_block_size,
    );
    // BOT classification reads at least the bootstrap block and may probe past
    // it for data. Rewind again so a fresh init overwrites the bootstrap at
    // block 0 rather than appending a new bootstrap after the probe reads.
    drive
        .rewind()
        .map_err(|error| format!("rewind before bootstrap write: {error}"))?;
    let action = {
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
        .map_err(|error| format!("apply init write gate: {error}"))?
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
            let mut archive = open_dump_archive(format, reader);
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
            let mut archive = open_dump_archive(format, reader);
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
            let mut archive = open_dump_archive(format, reader);
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
        ArchiveCommand::Inspect(_) => {
            unreachable!("archive inspect dispatched before the dump handler")
        }
        ArchiveCommand::Extract(_) => {
            unreachable!("archive extract dispatched before the dump handler")
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
        ArchiveFormat::Bru => BruFormat.probe_dump(reader),
    }
}

fn open_dump_archive(format: ArchiveFormat, reader: BufReader<File>) -> Box<dyn ArchiveReader> {
    match format {
        ArchiveFormat::Bru => Box::new(BruFormat.open_dump_reader(reader)),
    }
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

#[derive(Debug, Clone)]
struct ArchiveBuildInputFile {
    source_path: PathBuf,
    entry_type: RemTarEntryType,
    archive_path: String,
    file_id: String,
    size_bytes: u64,
    file_sha256: Option<[u8; 32]>,
    link_target: Option<String>,
}

fn build_archive_object_file(args: &ArchiveBuildArgs) -> Result<Value, String> {
    let tuning = archive_scan_tuning(args)?;
    if args.manifest_out.is_some() && args.rules.is_none() {
        return Err(
            "--manifest-out requires --rules so exclusions and wrapper members are recorded"
                .to_string(),
        );
    }
    if args.scan_only {
        if args.encrypt || args.key_file.is_some() || args.key_id.is_some() {
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
            tuning,
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
    if args.encrypt {
        object_id_field(&object_id)
            .map_err(|error| format!("--object-id is invalid for encrypted RAO: {error}"))?;
    }

    let materialized = if args.rules.is_some() {
        Some(archive_ingest::materialize_inputs(
            &args.inputs,
            args.rules.as_deref(),
            args.no_index,
            tuning,
        )?)
    } else {
        None
    };
    let inputs = match &materialized {
        Some(plan) => plan.inputs.clone(),
        None => collect_archive_build_inputs(&args.inputs)?,
    };
    if inputs.is_empty() {
        return Err("--inputs did not contain any archivable entries".to_string());
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
        if args.encrypt {
            let key_path = args
                .key_file
                .as_ref()
                .ok_or_else(|| "--encrypt requires --key-file".to_string())?;
            let root_key = read_root_key_file(key_path)?;
            let key_id = parse_key_id(args.key_id.as_deref())?;
            let mut readers = open_archive_build_readers(&inputs)?;
            let mut streams = archive_build_streams(&inputs, &mut readers);
            let report = write_encrypted_rao_object_from_readers(
                &mut sink,
                &options,
                &mut streams,
                &root_key,
                key_id,
            )
            .map_err(|error| format!("write encrypted RAO: {error}"))?;
            sink.sync_all()
                .map_err(|error| format!("sync {}: {error}", temp_path.display()))?;
            Ok(ArchiveBuildResult {
                layout: report.plaintext_layout,
                representation: "encrypted",
                encryption: "RAO1",
                key_id: Some(key_id),
                stored_digest: report.envelope.stored_digest,
                plaintext_digest: report.envelope.plaintext.digest,
                stored_size_bytes: report.envelope.stored_size_bytes,
                stored_size_blocks: report.envelope.stored_size_blocks,
            })
        } else {
            if args.key_file.is_some() || args.key_id.is_some() {
                return Err("--key-file/--key-id require --encrypt".to_string());
            }
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
                key_id: None,
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
    }

    Ok(archive_build_report_json(
        args,
        &inputs,
        &build,
        materialized.as_ref(),
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
        inspect_encrypted_archive_object_file(&args.object, args.key_file.as_deref())
    } else {
        if args.key_file.is_some() {
            return Err("--key-file is only valid for encrypted RAO objects".to_string());
        }
        inspect_plaintext_archive_object_file(&args.object, args.chunk_size)
    }
}

fn extract_archive_object_file(args: &ArchiveExtractArgs) -> Result<Value, String> {
    if archive_object_is_encrypted(&args.object)? {
        extract_encrypted_archive_object_file(args)
    } else {
        if args.key_file.is_some() {
            return Err("--key-file is only valid for encrypted RAO objects".to_string());
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

fn inspect_encrypted_archive_object_file(
    path: &Path,
    key_file: Option<&Path>,
) -> Result<Value, String> {
    let encrypted = read_archive_object_bytes(path)?;
    let inspected =
        inspect_bytes(&encrypted).map_err(|error| format!("inspect encrypted RAO: {error}"))?;
    let mut envelope_json = encrypted_archive_keyless_json(path, &inspected);
    if let Some(key_path) = key_file {
        let root_key = read_root_key_file(key_path)?;
        let chunk_size = usize::try_from(inspected.header.chunk_size)
            .map_err(|_| "encrypted header chunk_size is too large for this host".to_string())?;
        let mut source = FileBlockSource::open(path, chunk_size)
            .map_err(|error| format!("open {}: {error}", path.display()))?;
        let block_count = source.block_count();
        let opened = remanence_format::read_encrypted_rao_object(
            &mut source,
            chunk_size,
            block_count,
            &root_key,
        )
        .map_err(|error| format!("open encrypted RAO: {error}"))?;
        envelope_json["keyed"] = json!(true);
        envelope_json["plaintext_digest"] = json!(bytes_to_hex(&opened.envelope.plaintext.digest));
        envelope_json["plaintext_size_bytes"] = json!(opened.envelope.plaintext.size);
        envelope_json["plaintext"] = plaintext_archive_inspect_json(
            path,
            &opened.object,
            chunk_size,
            opened.envelope.metadata.plaintext_size,
            opened.envelope.metadata.plaintext_size / chunk_size as u64,
            opened.envelope.plaintext.digest,
        );
    }
    Ok(envelope_json)
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
                key_id: None,
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
        key_id: None,
        chunk_size,
        stored_size_bytes,
        stored_size_blocks: block_count,
        stored_digest,
    };
    let mut value = archive_extract_report_json(&context, &report);
    attach_unwrap_report(&mut value, &args.dest, args.overwrite, args.no_unwrap)?;
    Ok(value)
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
    let key_path = args
        .key_file
        .as_deref()
        .ok_or_else(|| "encrypted RAO extract requires --key-file".to_string())?;
    let encrypted = read_archive_object_bytes(&args.object)?;
    let inspected =
        inspect_bytes(&encrypted).map_err(|error| format!("inspect encrypted RAO: {error}"))?;
    let root_key = read_root_key_file(key_path)?;
    let (plaintext, envelope) = open_to_vec(&encrypted, &root_key)
        .map_err(|error| format!("open encrypted RAO: {error}"))?;
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
        key_id: Some(envelope.header.key_id),
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
    let key_path = args
        .key_file
        .as_deref()
        .ok_or_else(|| "encrypted RAO blob-member extract requires --key-file".to_string())?;
    let encrypted = read_archive_object_bytes(&args.object)?;
    let inspected =
        inspect_bytes(&encrypted).map_err(|error| format!("inspect encrypted RAO: {error}"))?;
    let root_key = read_root_key_file(key_path)?;
    let (plaintext, envelope) = open_to_vec(&encrypted, &root_key)
        .map_err(|error| format!("open encrypted RAO: {error}"))?;
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
        &root_key,
        &scan,
        BlobMemberExtractContext {
            representation: "encrypted",
            encryption: "RAO1",
            key_id: Some(envelope.header.key_id),
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
    key_id: Option<[u8; 16]>,
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
        "key_id": context.base.key_id.map(|key_id| bytes_to_hex(&key_id)),
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
    let output = write_archive_range_output(&args.dest, member_path, &bytes, args.overwrite)?;
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
    root_key: &RootKey,
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

    let idx_range = read_encrypted_rao_file_range_to_vec(
        encrypted,
        root_key,
        idx_entry.first_chunk_lba,
        idx_entry.size_bytes,
        0,
        idx_entry.size_bytes,
    )
    .map_err(|error| format!("extract encrypted RAO blob index range: {error}"))?;
    verify_locator_sha256(idx_entry, &idx_range.bytes, &idx_path)?;
    let member =
        archive_ingest::resolve_blob_member_from_index(&idx_range.bytes, &idx_path, member_path)?;
    let blob_range = read_encrypted_rao_file_range_to_vec(
        encrypted,
        root_key,
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
        write_archive_range_output(&args.dest, member_path, &blob_range.bytes, args.overwrite)?;
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
        key_id: None,
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
    let key_path = args
        .key_file
        .as_deref()
        .ok_or_else(|| "encrypted RAO range extract requires --key-file".to_string())?;
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
    let root_key = read_root_key_file(key_path)?;
    let range_result = read_encrypted_rao_file_range_to_vec(
        &encrypted,
        &root_key,
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
        key_id: Some(inspected.header.key_id),
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
        "chunk_size": report.header.chunk_size,
        "key_id": bytes_to_hex(&report.header.key_id),
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
    json!({
        "entry_type": archive_entry_type_name(entry.entry_type),
        "path": entry.path,
        "size_bytes": entry.size_bytes,
        "file_id": entry.pax_records.get("REMANENCE.file_id"),
        "file_sha256": entry.pax_records.get("REMANENCE.file_sha256"),
        "link_target": entry.link_target,
        "first_chunk_lba": entry.first_chunk_lba.map(|lba| lba.0),
        "chunk_count": entry.chunk_count,
        "data_offset": entry.data_offset,
    })
}

struct ArchiveExtractReportContext<'a> {
    object: &'a Path,
    dest: &'a Path,
    representation: &'static str,
    encryption: &'static str,
    key_id: Option<[u8; 16]>,
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
        "key_id": context.key_id.map(|key_id| bytes_to_hex(&key_id)),
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
    key_id: Option<[u8; 16]>,
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
        "key_id": context.key_id.map(|key_id| bytes_to_hex(&key_id)),
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
    let mut options = OpenOptions::new();
    options.write(true);
    if overwrite {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options
        .open(&destination)
        .map_err(|error| format!("open range output {}: {error}", destination.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("write range output {}: {error}", destination.display()))?;
    Ok(destination)
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
    key_id: Option<[u8; 16]>,
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
        "key_id": build.key_id.map(|key_id| bytes_to_hex(&key_id)),
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
        if let Some(path) = &args.manifest_out {
            report["manifest_out"] = json!(path);
        }
    }
    report
}

fn archive_file_report_json(layout: &RemTarFileLayout, input: &ArchiveBuildInputFile) -> Value {
    json!({
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
    })
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

fn read_root_key_file(path: &Path) -> Result<RootKey, String> {
    let mut bytes = std::fs::read(path)
        .map_err(|error| format!("read --key-file {}: {error}", path.display()))?;
    if bytes.len() != 32 {
        let len = bytes.len();
        bytes.zeroize();
        return Err(format!(
            "--key-file must contain exactly 32 bytes, got {len}"
        ));
    }
    RootKey::new(bytes).map_err(|error| error.to_string())
}

fn parse_key_id(value: Option<&str>) -> Result<[u8; 16], String> {
    let value = value.ok_or_else(|| "--encrypt requires --key-id".to_string())?;
    let bytes = hex_to_bytes(value)?;
    <[u8; 16]>::try_from(bytes)
        .map_err(|bytes| format!("--key-id must decode to 16 bytes, got {}", bytes.len()))
}

fn hex_to_bytes(value: &str) -> Result<Vec<u8>, String> {
    let value = value.trim();
    if value.len() % 2 != 0 {
        return Err("hex string must have even length".to_string());
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    let mut chars = value.as_bytes().chunks_exact(2);
    for pair in &mut chars {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_nibble(byte: u8) -> Result<u8, String> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(format!("invalid hex character {:?}", char::from(byte))),
    }
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
    let mut handle = match lib.open(policy) {
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
        ArchiveCommand::Inspect(_) => {
            unreachable!("archive inspect dispatched before the tape archive handler")
        }
        ArchiveCommand::Extract(_) => {
            unreachable!("archive extract dispatched before the tape archive handler")
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
        ArchiveFormat::Bru => BruFormat.probe(source),
    }
}

#[cfg(target_os = "linux")]
fn open_tape_archive<'a>(
    format: ArchiveFormat,
    source: &'a mut dyn remanence_library::PhysicalTapeSource,
    probe: &ProbeResult,
) -> Result<Box<dyn ArchiveReader + 'a>, FormatError> {
    match format {
        ArchiveFormat::Bru => BruFormat.open_tape_reader(source, probe),
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
    let mut handle = match lib.open(policy) {
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
                    let _ = writeln!(
                        out,
                        "    [0x{addr:04x}] {dv} {dp} {dr}  {dsg}  serial {serial}",
                        addr = bay.element_address,
                        serial = installed.serial,
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "    [0x{addr:04x}] (no identity — see warnings)",
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
    if slot.full {
        let tag = slot.cartridge.as_deref().unwrap_or("(no voltag)");
        let suffix = if tag.starts_with("CLN") {
            "   (cleaning)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "  [0x{:04x}] full   {tag}{suffix}",
            slot.element_address
        );
    } else {
        let _ = writeln!(out, "  [0x{:04x}] empty", slot.element_address);
    }
}

fn print_ie(ie: &IePort, out: &mut dyn Write) {
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
        "    [0x{:04x}] {state}   (import:{in_} export:{out_})",
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
        scsi, DriveBay, ElementLayout, IdentitySource, InstalledDrive, Library, Slot,
    };
    use std::fs;
    use std::path::PathBuf;

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
                default_idle_timeout_seconds: 1800,
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
    fn rem_archive_help_hides_direct_tape_object_commands() {
        let mut command = Cli::command();
        let archive = command.find_subcommand_mut("archive").unwrap();
        let help = command_help(archive.clone());

        for command in ["write", "read", "verify"] {
            assert!(
                !help.contains(&format!("\n  {command}")),
                "rem archive help should not expose direct tape command {command}:\n{help}"
            );
        }
        for command in [
            "build", "inspect", "extract", "probe", "scan", "restore", "recover", "list",
        ] {
            assert!(
                help.contains(&format!("\n  {command}")),
                "rem archive help should expose {command}:\n{help}"
            );
        }
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
                assert!(!args.encrypt);
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
                assert!(args.key_file.is_none());
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
                assert!(args.key_file.is_none());
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
            "--key-file",
            "/tmp/root.key",
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
            "--encrypt",
            "--key-file",
            "/tmp/root.key",
            "--key-id",
            "42424242424242424242424242424242",
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
                assert!(args.encrypt);
                assert_eq!(args.key_file, Some(PathBuf::from("/tmp/root.key")));
                assert_eq!(
                    args.key_id.as_deref(),
                    Some("42424242424242424242424242424242")
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
            Command::Archive {
                command: ArchiveCommand::Read(_)
            }
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
            Command::Archive {
                command: ArchiveCommand::ExportObject(_)
            }
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
            Command::Archive {
                command: ArchiveCommand::Verify(_)
            }
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
            Command::Archive {
                command: ArchiveCommand::List(_)
            }
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

    #[test]
    fn rem_daemon_health_roundtrips_against_in_process_api_service() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-daemon-health")
            .tempdir()
            .unwrap();
        let index = remanence_state::CatalogIndex::open(temp.path().join("state.sqlite")).unwrap();
        let state = remanence_api::ApiState::new(index);
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                addr_tx.send(addr).unwrap();
                tonic::transport::Server::builder()
                    .add_service(pb::daemon_server::DaemonServer::new(state.daemon_service()))
                    .serve_with_incoming_shutdown(
                        tokio_stream::wrappers::TcpListenerStream::new(listener),
                        async {
                            let _ = shutdown_rx.await;
                        },
                    )
                    .await
                    .unwrap();
            });
        });

        let endpoint = format!("http://{}", addr_rx.recv().unwrap());
        let cli = Cli::parse_from(["rem", "daemon", "--endpoint", &endpoint, "health"]);
        let mut out = Vec::<u8>::new();
        let mut err = Vec::<u8>::new();
        let code = run(
            cli,
            move || -> Result<DiscoveryReport, DiscoveryError> {
                panic!("discover_fn must not be called for daemon client command")
            },
            &mut out,
            &mut err,
        );
        let _ = shutdown_tx.send(());
        server.join().unwrap();

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(
            err.is_empty(),
            "stderr should be empty: {}",
            String::from_utf8_lossy(&err)
        );
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("status: healthy"), "{stdout}");
        assert!(stdout.contains("sqlite_index: ok"), "{stdout}");
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

    fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle)
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
        assert!(report["key_id"].is_null());
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

    #[cfg(unix)]
    #[test]
    fn archive_build_rules_wraps_xattr_file_fallback() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-xattr-fallback")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        let xattr_file = input_dir.join("--xattr.txt");
        fs::write(&xattr_file, b"xattr payload").unwrap();
        let status = std::process::Command::new("setfattr")
            .arg("-n")
            .arg("user.remanence_test")
            .arg("-v")
            .arg("kept")
            .arg(&xattr_file)
            .status()
            .expect("setfattr must be installed for xattr fallback test");
        assert!(status.success());
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
        assert_eq!(report["ingest"]["scan"]["totals"]["wrapped_entries"], 1);
        assert!(report["ingest"]["scan"]["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cluster| cluster["reason"] == "xattr" && cluster["samples"][0] == "--xattr.txt"));
        let files = report["files"].as_array().expect("files array");
        assert!(files
            .iter()
            .any(|file| file["path"] == "--xattr.txt.remwrap.tar"));
        assert!(!files.iter().any(|file| file["path"] == "--xattr.txt"));

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
        let output = std::process::Command::new("getfattr")
            .arg("--absolute-names")
            .arg("--dump")
            .arg("-m")
            .arg("-")
            .arg(restore_dir.join("--xattr.txt"))
            .output()
            .expect("getfattr must be installed for xattr fallback test");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("user.remanence_test=\"kept\""), "{stdout}");
    }

    #[cfg(unix)]
    #[test]
    fn archive_build_rules_wraps_hardlink_common_ancestor() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-hardlink")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(input_dir.join("links")).unwrap();
        fs::write(input_dir.join("links/original.bin"), b"same-inode").unwrap();
        fs::hard_link(
            input_dir.join("links/original.bin"),
            input_dir.join("links/alias.bin"),
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
        assert_eq!(report["ingest"]["scan"]["totals"]["blob_entries"], 1);
        assert!(report["ingest"]["scan"]["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cluster| cluster["reason"] == "hardlink" && cluster["samples"][0] == "links"));
        let files = report["files"].as_array().expect("files array");
        assert!(files.iter().any(|file| file["path"] == "links.remwrap.tar"));
        assert!(files.iter().any(|file| file["path"] == "links.remwrap.idx"));
        assert!(!files
            .iter()
            .any(|file| file["path"] == "links/original.bin"));

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
        let original = fs::metadata(restore_dir.join("links/original.bin")).unwrap();
        let alias = fs::metadata(restore_dir.join("links/alias.bin")).unwrap();
        assert_eq!(original.ino(), alias.ino());
        assert_eq!(original.nlink(), 2);
    }

    #[test]
    fn archive_build_encrypted_bypasses_discovery_and_round_trips() {
        let temp = tempfile::Builder::new()
            .prefix("remanence-cli-rao-build-encrypted")
            .tempdir()
            .unwrap();
        let input_dir = temp.path().join("inputs");
        fs::create_dir_all(&input_dir).unwrap();
        fs::write(input_dir.join("secret.txt"), b"classified payload").unwrap();
        let key_path = temp.path().join("root.key");
        fs::write(&key_path, [0x42u8; 32]).unwrap();
        let out_path = temp.path().join("encrypted.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--encrypt",
            "--key-file",
            key_path.to_str().unwrap(),
            "--key-id",
            "24242424242424242424242424242424",
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

        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        assert!(out_path.exists());

        let report: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is json");
        assert_eq!(report["object_id"], "object-encrypted");
        assert_eq!(report["caller_object_id"], "caller-encrypted");
        assert_eq!(report["body_format"], "rao-v1");
        assert_eq!(report["representation"], "encrypted");
        assert_eq!(report["encryption"], "RAO1");
        assert_eq!(report["key_id"], "24242424242424242424242424242424");
        assert_eq!(report["chunk_size"], 4096);
        assert_ne!(report["stored_digest"], report["plaintext_digest"]);

        let stored = fs::read(&out_path).unwrap();
        assert!(!bytes_contain(&stored, b"secret.txt"));
        assert!(!bytes_contain(&stored, b"classified payload"));
        assert!(!bytes_contain(
            &stored,
            remanence_format::MANIFEST_PATH.as_bytes()
        ));

        let root_key = RootKey::new([0x42; 32]).unwrap();
        let mut source = remanence_library::FileBlockSource::open(&out_path, 4096).unwrap();
        let block_count = source.block_count();
        assert_eq!(report["stored_size_blocks"].as_u64().unwrap(), block_count);
        let read =
            remanence_format::read_encrypted_rao_object(&mut source, 4096, block_count, &root_key)
                .unwrap();
        assert_eq!(
            read.object.entry("secret.txt").unwrap().data,
            b"classified payload"
        );
        assert_eq!(
            bytes_to_hex(&read.envelope.metadata.plaintext_digest),
            report["plaintext_digest"]
        );

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "inspect",
            "--object",
            out_path.to_str().unwrap(),
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let keyless: serde_json::Value = serde_json::from_str(&stdout).expect("keyless json");
        assert_eq!(keyless["representation"], "encrypted");
        assert_eq!(keyless["keyed"], false);
        assert_eq!(keyless["object_id"], "object-encrypted");
        assert_eq!(keyless["key_id"], "24242424242424242424242424242424");
        assert!(keyless.get("plaintext").is_none());

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "inspect",
            "--object",
            out_path.to_str().unwrap(),
            "--key-file",
            key_path.to_str().unwrap(),
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let keyed: serde_json::Value = serde_json::from_str(&stdout).expect("keyed json");
        assert_eq!(keyed["keyed"], true);
        assert_eq!(keyed["plaintext"]["object_id"], "object-encrypted");
        assert_eq!(keyed["plaintext"]["files"][0]["path"], "secret.txt");
        assert_eq!(keyed["plaintext_digest"], report["plaintext_digest"]);

        let restore_dir = temp.path().join("encrypted-restore");
        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "extract",
            "--object",
            out_path.to_str().unwrap(),
            "--dest",
            restore_dir.to_str().unwrap(),
            "--key-file",
            key_path.to_str().unwrap(),
        ]);
        assert_eq!(format!("{code:?}"), format!("{:?}", ExitCode::SUCCESS));
        assert!(stderr.is_empty(), "{stderr}");
        let extract: serde_json::Value = serde_json::from_str(&stdout).expect("extract json");
        assert_eq!(extract["representation"], "encrypted");
        assert_eq!(extract["files_written"], 1);
        assert_eq!(extract["bytes_written"], b"classified payload".len() as u64);
        assert_eq!(
            fs::read(restore_dir.join("secret.txt")).unwrap(),
            b"classified payload"
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
        let key_path = temp.path().join("root.key");
        fs::write(&key_path, [0x24u8; 32]).unwrap();
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
            "--encrypt",
            "--key-file",
            key_path.to_str().unwrap(),
            "--key-id",
            "55555555555555555555555555555555",
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
            "--key-file",
            key_path.to_str().unwrap(),
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
        let key_path = temp.path().join("root.key");
        fs::write(&key_path, [0x42u8; 32]).unwrap();
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
            "--encrypt".to_string(),
            "--key-file".to_string(),
            key_path.to_str().unwrap().to_string(),
            "--key-id".to_string(),
            "42424242424242424242424242424242".to_string(),
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
        let key_path = temp.path().join("root.key");
        fs::write(&key_path, [0x52u8; 32]).unwrap();
        let out_path = temp.path().join("encrypted-range.rao");

        let (code, stdout, stderr) = invoke_without_discovery(&[
            "rem",
            "archive",
            "build",
            "--inputs",
            input_dir.to_str().unwrap(),
            "--out",
            out_path.to_str().unwrap(),
            "--encrypt",
            "--key-file",
            key_path.to_str().unwrap(),
            "--key-id",
            "52525252525252525252525252525252",
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
            inspected.header.metadata_frame_len,
            512,
            unrequested_chunk,
        )
        .unwrap() as usize;
        stored[corrupt_offset] ^= 0x40;
        fs::write(&out_path, &stored).unwrap();

        let root_key = RootKey::new([0x52; 32]).unwrap();
        let mut full_source = remanence_library::FileBlockSource::open(&out_path, 512).unwrap();
        let full_blocks = full_source.block_count();
        assert!(
            remanence_format::read_encrypted_rao_object(
                &mut full_source,
                512,
                full_blocks,
                &root_key
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
            "--key-file".to_string(),
            key_path.to_str().unwrap().to_string(),
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
            installed: None,
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }];
        lib.slots = vec![
            Slot {
                element_address: 1000,
                full: true,
                cartridge: Some("L00001".into()),
            },
            Slot {
                element_address: 1001,
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
            installed: None,
            loaded: false,
            loaded_tape: None,
            source_slot: None,
        }];
        lib.slots = vec![Slot {
            element_address: 1000,
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
                full: true,
                cartridge: Some("CLNU01L9".into()),
            },
            Slot {
                element_address: 0x03ea,
                full: true,
                cartridge: Some("S20001L9".into()),
            },
            Slot {
                element_address: 0x03eb,
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
    const MAGIC_FILE_HEADER: u64 = 0x2345;
    const ARTIME_OFFSET: usize = 0x098;
    const BUFSIZE_OFFSET: usize = 0x0A0;
    const RELEASE_MINOR_OFFSET: usize = 0x0B8;
    const RELEASE_MAJOR_OFFSET: usize = 0x0BC;
    const VARIANT_OFFSET: usize = 0x0C0;
    const ARCHIVE_ID_LOW_OFFSET: usize = 0x0D8;
    const LABEL_OFFSET: usize = 0x1D0;
    const FILE_PATH_OFFSET: usize = 0x000;
    const INLINE_DATA_LEN_OFFSET: usize = 0x0DC;
    const INLINE_DATA_OFFSET: usize = 0x400;
    const ST_MODE_OFFSET: usize = 0x180;
    const ST_SIZE_OFFSET: usize = 0x1B8;
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
