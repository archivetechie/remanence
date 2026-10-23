//! Production standalone dispatch. Dependencies are filesystem roots, enumeration
//! and transport only; the public integration seam runs this same parser and path.
use clap::{Parser, Subcommand};
use remanence_api::{MountBackend, MountTarget, OperatorMount};
use remanence_library::{
    BlockSize, DeviceAttachment, DiscoveryReport, DriveHandle, DriveHandleSource, IoErrorKind,
    SgTransport, StaticAllowlist,
};
use remanence_state::{
    GenerationSource, LabelProvenance, MediaObservation, StateHandle, StatePaths,
};
use serde_json::{json, Value};
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::ExitCode,
};
use uuid::Uuid;

// One-cartridge invocations close a parity epoch at each checkpoint. Keep the
// same RS(128,4) code with one stripe so a small home write does not emit the
// library default's 512 MiB sidecar. Operators can select --scheme default.
const HOME_PARITY_HELP: &str = "New home-mode CLI pools default to custom:128,4,1. At 256 KiB blocks, contiguous-loss tolerance is 1 MiB versus 512 MiB for the library geometry, reducing parity overhead for small writes. Use rem pool new NAME --scheme default to select the block-size-aware library geometry, then rem tape init --pool NAME.";
const HOME_POOL_SCHEME: &str = "custom:128,4,1";

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

#[derive(Debug, Parser)]
#[command(
    name = "rem",
    about = "library operations go through the daemon; a standalone drive is operated directly."
)]
struct Cli {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    device: Option<PathBuf>,
    #[arg(long, global = true)]
    allow_device: Vec<String>,
    #[arg(long, global = true)]
    allow_unserialized_device: bool,
    #[arg(long, global = true)]
    allow_untrusted_state_volume: bool,
    #[arg(long, global = true)]
    acknowledge_unknown_ownership: bool,
    #[arg(long, global = true)]
    reserve: bool,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Subcommand)]
enum Command {
    List {
        #[arg(long, alias = "tape-uuid")]
        tape: Option<String>,
        #[arg(long)]
        path_prefix: Option<String>,
    },
    Doctor,
    Drives,
    Diagnostics {
        #[command(subcommand)]
        command: Diagnostics,
    },
    Pool {
        #[command(subcommand)]
        command: Pool,
    },
    Tape {
        #[command(subcommand)]
        command: Tape,
    },
    Put {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
        pool: Option<String>,
        #[arg(long)]
        recipient: Vec<PathBuf>,
        /// Permit a plaintext version when the newest committed version is encrypted.
        #[arg(long)]
        allow_plaintext_copy: bool,
        #[arg(long, alias = "tape-uuid")]
        tape: Option<String>,
    },
    Get {
        object: String,
        #[arg(long)]
        version: Option<Uuid>,
        #[arg(long, alias = "dest")]
        out: PathBuf,
        #[arg(long)]
        private_key: Option<PathBuf>,
    },
}
#[derive(Debug, Subcommand)]
enum Diagnostics {
    Capture {
        #[arg(long)]
        out: PathBuf,
    },
}
#[derive(Debug, Subcommand)]
enum Pool {
    #[command(after_help = HOME_PARITY_HELP)]
    New {
        name: String,
        #[arg(long)]
        scheme: Option<String>,
        #[arg(long, value_parser=crate::recovery_report::parse_block_size)]
        block_size: Option<u32>,
    },
}
#[derive(Debug, Subcommand)]
enum Quarantine {
    List,
    Release {
        id: String,
        #[arg(long)]
        ack: String,
    },
}
#[derive(Debug, Subcommand)]
enum Tape {
    Quarantine {
        #[command(subcommand)]
        command: Quarantine,
    },
    Identify,
    #[command(after_help = HOME_PARITY_HELP)]
    Init {
        #[arg(long)]
        label: String,
        #[arg(long)]
        pool: Option<String>,
        #[arg(long)]
        generation: Option<String>,
        #[arg(long, value_parser=crate::recovery_report::parse_block_size)]
        block_size: Option<u32>,
        /// Erase foreign or existing data after exact label confirmation.
        #[arg(long)]
        clobber_data: bool,
        /// Non-interactive confirmation: CLOBBER followed by the exact label.
        #[arg(long, requires = "clobber_data")]
        confirm_clobber: Option<String>,
    },
    Adopt {
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        pool: Option<String>,
    },
    Verify {
        #[arg(long)]
        full: bool,
    },
    Restore {
        #[arg(long, alias = "dest", required_unless_present = "list")]
        out: Option<PathBuf>,
        #[arg(long)]
        list: bool,
        #[arg(long)]
        path: Vec<String>,
        #[arg(long)]
        private_key: Option<PathBuf>,
        #[arg(long)]
        scan: bool,
    },
    Finalize {
        #[arg(long)]
        tape: Uuid,
        #[arg(long)]
        acknowledge_tape: Uuid,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        idempotency_key: Uuid,
        #[arg(long)]
        expected_pool: Option<String>,
    },
    AssignPool {
        #[arg(long)]
        tape: Option<Uuid>,
        #[arg(
            long,
            conflicts_with = "unassign",
            required_unless_present = "unassign"
        )]
        pool: Option<String>,
        #[arg(long)]
        unassign: bool,
    },
    Rebind {
        #[arg(long)]
        label: String,
        #[arg(long)]
        previous_uuid: Uuid,
    },
    SetGeneration {
        #[arg(long)]
        generation: String,
    },
    SupersedeTail {
        #[arg(long)]
        acknowledge_tape: String,
        #[arg(long)]
        confirm_supersede: Option<String>,
    },
    ImportJournals {
        #[arg(long)]
        from: PathBuf,
    },
    Release,
}

/// Select explicit standalone operations before the legacy library discovery path.
pub(crate) fn selected(args: &[OsString]) -> bool {
    selected_in(args, &Environment::from_process())
}

fn selected_in(args: &[OsString], env: &Environment) -> bool {
    let words: Vec<_> = args.iter().skip(1).map(|s| s.to_string_lossy()).collect();
    if words
        .iter()
        .take_while(|s| s.as_ref() != "--")
        .any(|s| s == "--device" || s.starts_with("--device="))
    {
        return true;
    }
    let mut i = 0;
    while let Some(word) = words.get(i) {
        if matches!(
            word.as_ref(),
            "--config" | "--allow" | "--allow-derived" | "--allow-device"
        ) {
            i += 2;
            continue;
        }
        if word.starts_with('-') {
            i += 1;
            continue;
        }
        if word == "help" {
            i += 1;
            // Clap recognizes help only at a command boundary.
            if words.get(i).is_some_and(|w| w == "help") {
                return false;
            }
            continue;
        }
        return match word.as_ref() {
            "doctor" | "drives" | "diagnostics" | "list" => true,
            "pool" => words.get(i + 1).is_some_and(|w| w == "new"),
            "tape" if words.get(i + 1).is_some_and(|w| w == "quarantine") => {
                words
                    .get(i + 2)
                    .is_some_and(|w| matches!(w.as_ref(), "list" | "release"))
                    && !env.system_config.exists()
                    && !words.iter().any(|w| {
                        w == "--config"
                            || w.starts_with("--config=")
                            || w == "--endpoint"
                            || w.starts_with("--endpoint=")
                    })
                    && load(&env.xdg.join("rem/config.toml"), true)
                        .ok()
                        .and_then(|config| config.home)
                        .and_then(|home| home.drive_serial)
                        .is_some()
                    && !words.iter().any(|w| {
                        w == "--library"
                            || w.starts_with("--library=")
                            || w == "--after-settled-inventory"
                    })
            }
            "tape" => words.get(i + 1).is_some_and(|w| {
                matches!(
                    w.as_ref(),
                    "identify"
                        | "verify"
                        | "adopt"
                        | "restore"
                        | "assign-pool"
                        | "rebind"
                        | "set-generation"
                        | "import-journals"
                        | "release"
                )
            }),
            _ => false,
        };
    }
    false
}
/// Ask Clap whether help was requested syntactically. A positional file/object
/// named `help`, an option value, and words after `--` are ordinary operands.
/// Shared commands display both interfaces without changing their runtime routing.
pub(crate) fn shared_help(args: &[OsString]) -> Option<String> {
    use clap::{error::ErrorKind, CommandFactory};
    let libraries = crate::Cli::command();
    let standalone = Cli::command();
    let requested = [&libraries, &standalone].into_iter().any(|command| {
        command
            .clone()
            .try_get_matches_from(args)
            .is_err_and(|e| e.kind() == ErrorKind::DisplayHelp)
    });
    if !requested {
        return None;
    }
    // Only inspect the command prefix. Values of global options precede it;
    // operands and command-local options cannot select a different help target.
    let mut words = args.iter().skip(1).map(|s| s.to_string_lossy());
    let first = loop {
        let word = words.next()?;
        if matches!(
            word.as_ref(),
            "--config" | "--allow" | "--allow-derived" | "--allow-device" | "--device"
        ) {
            words.next()?;
            continue;
        }
        if word == "--" {
            return None;
        }
        if word.starts_with('-') {
            continue;
        }
        break word;
    };
    let first = if first == "help" {
        words.next()?
    } else {
        first
    };
    let path = match first.as_ref() {
        "put" => vec!["put"],
        "get" => vec!["get"],
        "tape" => {
            let child = loop {
                let word = words.next()?;
                if matches!(
                    word.as_ref(),
                    "--config" | "--allow" | "--allow-derived" | "--allow-device" | "--device"
                ) {
                    words.next()?;
                    continue;
                }
                if word == "help" || word.starts_with('-') {
                    continue;
                }
                break word;
            };
            match child.as_ref() {
                "init" => vec!["tape", "init"],
                "finalize" => vec!["tape", "finalize"],
                _ => return None,
            }
        }
        _ => return None,
    };
    fn render(mut root: clap::Command, path: &[&str]) -> String {
        root.build();
        let mut command = &mut root;
        for name in path {
            command = command.find_subcommand_mut(name).expect("shared command");
        }
        command.set_bin_name(format!("rem {}", path.join(" ")));
        command.render_long_help().to_string()
    }
    Some(format!(
        "Library/daemon interface:\n{}\nStandalone device interface:\n{}",
        render(libraries, &path),
        render(standalone, &path)
    ))
}

struct Environment {
    xdg: PathBuf,
    system_config: PathBuf,
    locks: PathBuf,
    test_input: bool,
}
impl Environment {
    fn from_process() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Environment {
            xdg: std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".local/state")),
            system_config: "/etc/rem/config.toml".into(),
            locks: "/run/lock/rem".into(),
            test_input: false,
        }
    }
}
pub(crate) fn run_live(args: &[OsString]) -> ExitCode {
    let env = Environment::from_process();
    let enumeration = || remanence_library::sysfs::enumerate_sg_devices();
    let mut factory = |path: &Path| {
        remanence_library::LinuxSgTransport::open_rw(path)
            .map(|t| Box::new(t) as Box<dyn SgTransport>)
            .map_err(|e| IoErrorKind::from(&e))
    };
    run(
        args,
        env,
        enumeration,
        &mut factory,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    )
}

/// Invoke production parsing/dispatch with isolated paths and real injected SCSI IO.
#[allow(clippy::too_many_arguments)]
pub fn run_with_standalone_test_io<F>(
    args: &[&str],
    home: &Path,
    xdg: &Path,
    system_config: &Path,
    locks: &Path,
    devices: Vec<DeviceAttachment>,
    factory: F,
    out: &mut dyn Write,
    errors: &mut dyn Write,
) -> ExitCode
where
    F: FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>,
{
    run_with_standalone_test_input(
        args,
        home,
        xdg,
        system_config,
        locks,
        devices,
        factory,
        out,
        errors,
        false,
    )
}

/// Invoke the same dispatcher with terminal input explicitly enabled for PTY tests.
#[allow(clippy::too_many_arguments)]
pub fn run_with_standalone_test_input<F>(
    args: &[&str],
    home: &Path,
    xdg: &Path,
    system_config: &Path,
    locks: &Path,
    devices: Vec<DeviceAttachment>,
    mut factory: F,
    out: &mut dyn Write,
    errors: &mut dyn Write,
    terminal_input: bool,
) -> ExitCode
where
    F: FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>,
{
    let env = Environment {
        xdg: if xdg.as_os_str().is_empty() {
            home.join(".local/state")
        } else {
            xdg.into()
        },
        system_config: system_config.into(),
        locks: locks.into(),
        test_input: !terminal_input,
    };
    run(
        &args.iter().map(OsString::from).collect::<Vec<_>>(),
        env,
        || {
            Ok(remanence_library::sysfs::DeviceEnumeration {
                devices,
                warnings: vec![],
            })
        },
        &mut factory,
        out,
        errors,
    )
}

/// Like [`run_with_standalone_test_io`], with a full host enumeration so tests
/// can supply the walker's own warnings (for example skipped non-tape nodes).
#[allow(clippy::too_many_arguments)]
pub fn run_with_standalone_test_enumeration<F>(
    args: &[&str],
    home: &Path,
    xdg: &Path,
    system_config: &Path,
    locks: &Path,
    enumeration: remanence_library::sysfs::DeviceEnumeration,
    mut factory: F,
    out: &mut dyn Write,
    errors: &mut dyn Write,
) -> ExitCode
where
    F: FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>,
{
    let env = Environment {
        xdg: if xdg.as_os_str().is_empty() {
            home.join(".local/state")
        } else {
            xdg.into()
        },
        system_config: system_config.into(),
        locks: locks.into(),
        test_input: true,
    };
    run(
        &args.iter().map(OsString::from).collect::<Vec<_>>(),
        env,
        || Ok(enumeration),
        &mut factory,
        out,
        errors,
    )
}
fn run<F, E>(
    args: &[OsString],
    env: Environment,
    enumeration: E,
    factory: &mut F,
    out: &mut dyn Write,
    errors: &mut dyn Write,
) -> ExitCode
where
    F: FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>,
    E: FnOnce() -> Result<remanence_library::sysfs::DeviceEnumeration, IoErrorKind>,
{
    if let Some(help) = shared_help(args) {
        let _ = writeln!(out, "{help}");
        return ExitCode::SUCCESS;
    }
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() { 2 } else { 0 };
            if e.use_stderr() {
                let _ = write!(errors, "{e}");
            } else {
                let _ = write!(out, "{e}");
            }
            return ExitCode::from(code);
        }
    };
    let mut observed = vec![];
    let mut output = PutOutput {
        active_fences: Vec::new(),
        out,
        json: cli.json,
        receipts: Vec::new(),
        summary: None,
        tape_continuation: false,
    };
    let mut result = dispatch(
        &cli,
        &env,
        enumeration,
        factory,
        &mut observed,
        errors,
        &mut output,
    );
    if let Err(error) = &result {
        if matches!(cli.command, Command::Put { .. }) && output.summary.is_none() {
            let _ = output.finish(Some(&(PathBuf::new(), error.clone())), "");
        }
    }
    let receipts = std::mem::take(&mut output.receipts);
    let summary = output.summary.take();
    let active_fences = std::mem::take(&mut output.active_fences);
    drop(output);
    if read_only(&cli.command) {
        if let Ok(value) = &mut result {
            value["active_fences"] = json!(active_fences);
        }
    }
    let incomplete_finalization = matches!(
        cli.command,
        Command::Tape {
            command: Tape::Finalize { .. }
        }
    ) && result.as_ref().is_ok_and(|v| v["finalized"] == false);
    let value = match &result {
        Ok(v) if incomplete_finalization => {
            json!({"ok":false,"status":"failed","error":format!("finalization outcome: {}", v["outcome"].as_str().unwrap_or("unknown")),"result":v})
        }
        Ok(v) => json!({"ok":true,"result":v}),
        Err(e) if matches!(cli.command, Command::Put { .. }) => {
            json!({"ok":false,"status":"failed","error":e,"result":{"receipts":receipts,"summary":summary}})
        }
        Err(e) => json!({"ok":false,"status":"failed","error":e}),
    };
    if cli.json {
        let mut value = value;
        value["supported_endpoint_kinds"] = json!(["library", "standalone"]);
        value["observed_endpoints"] = json!(observed);
        value["active_fences"] = json!(active_fences);
        let _ = writeln!(out, "{value}");
    } else {
        match &result {
            Ok(v) if matches!(cli.command, Command::List { .. }) => {
                let _ = writeln!(
                    out,
                    "POOL  PATH  SIZE  VERSIONS  NEWEST OBJECT  TAPES  FINALIZED"
                );
                for row in v["rows"].as_array().into_iter().flatten() {
                    let labels = row["tapes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = writeln!(
                        out,
                        "{}  {}  {}  {}  {}  {}  {}",
                        row["pool"].as_str().unwrap(),
                        row["path"].as_str().unwrap(),
                        row["size"],
                        row["version_count"],
                        row["newest_object_id"].as_str().unwrap(),
                        labels,
                        row["finalized"]
                    );
                }
            }
            Ok(v) if !matches!(cli.command, Command::Put { .. }) => {
                let _ = writeln!(out, "{}", serde_json::to_string_pretty(v).unwrap());
            }
            Err(e) => {
                let _ = writeln!(errors, "error: {e}");
            }
            Ok(_) => {}
        }
    }
    if result.is_ok() && !incomplete_finalization {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
fn read_only(command: &Command) -> bool {
    matches!(
        command,
        Command::List { .. }
            | Command::Doctor
            | Command::Drives
            | Command::Diagnostics { .. }
            | Command::Get { .. }
            | Command::Tape {
                command: Tape::Identify | Tape::Verify { .. } | Tape::Restore { .. }
            }
    )
}

fn config_path(cli: &Cli, env: &Environment) -> Result<PathBuf, String> {
    if let Some(p) = &cli.config {
        return fs::canonicalize(p).map_err(|e| format!("config {}: {e}", p.display()));
    }
    if env.system_config.try_exists().map_err(err)? {
        return Ok(env.system_config.clone());
    }
    if !env.xdg.is_absolute() {
        return Err("XDG_STATE_HOME (or the HOME-derived default) must be absolute".into());
    }
    let root = env.xdg.join("rem");
    fs::create_dir_all(&root).map_err(err)?;
    let path = root.join("config.toml");
    if !path.try_exists().map_err(err)? {
        let q = |p: PathBuf| toml::Value::String(p.to_string_lossy().into_owned()).to_string();
        let text=format!("[daemon]\nstate_dir = {}\ndefault_idle_timeout_seconds = 1800\n[journal]\ndir = {}\n[audit]\ndir = {}\nfsync = true\n[index]\nsqlite_path = {}\n[cache]\ntape_catalog_dir = {}\n[[tape_pools]]\nid = \"default\"\nscheme = \"custom:128,4,1\"\n",q(root.clone()),q(root.join("journals")),q(root.join("audit")),q(root.join("index/rem-state.sqlite")),q(root.join("cache/tapes")));
        let mut f = tempfile::NamedTempFile::new_in(&root).map_err(err)?;
        f.write_all(text.as_bytes()).map_err(err)?;
        f.as_file().sync_all().map_err(err)?;
        match f.persist_noclobber(&path) {
            Ok(_) => {}
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(err(e)),
        }
        fs::File::open(&root)
            .and_then(|f| f.sync_all())
            .map_err(err)?;
    }
    Ok(path)
}
/// Local library quarantine commands share home-mode configuration resolution.
pub(crate) fn local_quarantine_config(
    path: &Path,
    errors: &mut dyn Write,
) -> Result<PathBuf, String> {
    local_quarantine_config_in(path, &Environment::from_process(), errors)
}

fn local_quarantine_config_in(
    path: &Path,
    env: &Environment,
    errors: &mut dyn Write,
) -> Result<PathBuf, String> {
    if path != env.system_config || path.try_exists().map_err(err)? {
        return Ok(path.to_owned());
    }
    if !env.xdg.is_absolute() {
        return Err("home state path must be absolute".into());
    }
    let path = env.xdg.join("rem/config.toml");
    writeln!(errors, "using config {}", path.display()).map_err(err)?;
    if !path.try_exists().map_err(err)? {
        return Err(format!("config {} does not exist", path.display()));
    }
    Ok(path)
}

fn load(path: &Path, override_trust: bool) -> Result<remanence_state::RemConfig, String> {
    let mut doc: toml::Value =
        toml::from_str(&fs::read_to_string(path).map_err(err)?).map_err(err)?;
    if override_trust {
        doc.get_mut("journal")
            .and_then(toml::Value::as_table_mut)
            .ok_or("missing journal config")?
            .insert("require_trusted_volume".into(), toml::Value::Boolean(false));
    }
    remanence_state::parse_config_toml(&toml::to_string(&doc).map_err(err)?).map_err(|e| {
        format!(
            "{e}; use --allow-untrusted-state-volume only to explicitly override the volume check"
        )
    })
}
fn assign(state: &mut StateHandle, uuid: [u8; 16], pool: Option<&str>) -> Result<(), String> {
    if let Some(p) = pool {
        if !state.config().tape_pools.iter().any(|v| v.id == p) {
            return Err(format!("unconfigured pool {p}"));
        }
    }
    let row = state
        .catalog_index()
        .get_tape(&uuid)
        .map_err(err)?
        .ok_or("tape is not catalogued")?;
    state
        .assign_tape_pool(uuid, row.assignment_generation, pool)
        .map_err(err)?;
    Ok(())
}
fn dispatch<F, E>(
    cli: &Cli,
    env: &Environment,
    enumeration: E,
    factory: &mut F,
    observed: &mut Vec<Value>,
    errors: &mut dyn Write,
    output: &mut PutOutput<'_>,
) -> Result<Value, String>
where
    F: FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>,
    E: FnOnce() -> Result<remanence_library::sysfs::DeviceEnumeration, IoErrorKind>,
{
    if cli.device.is_none()
        && matches!(
            cli.command,
            Command::Tape { .. } | Command::Put { .. } | Command::Get { .. }
        )
        && !matches!(
            cli.command,
            Command::Tape {
                command: Tape::AssignPool { tape: Some(_), .. } | Tape::Quarantine { .. }
            }
        )
    {
        return Err("this command requires --device PATH and --allow-device <serial|PATH>".into());
    }
    if cli.device.is_some() && cli.allow_device.is_empty() {
        return Err("--device requires explicit --allow-device <serial|PATH>".into());
    }
    if cli.allow_unserialized_device && (!read_only(&cli.command) || cli.reserve) {
        return Err(
            "--allow-unserialized-device is only permitted for read verbs without --reserve".into(),
        );
    }
    if let Command::List { tape, path_prefix } = &cli.command {
        if cli.device.is_some() {
            return Err("list does not take --device".into());
        }
        // Read-only inventory never creates home config, state, journals, or locks.
        let path = cli.config.clone().unwrap_or_else(|| {
            if env.system_config.exists() {
                env.system_config.clone()
            } else {
                env.xdg.join("rem/config.toml")
            }
        });
        if !path.exists() {
            if cli.config.is_some() || tape.is_some() {
                return Err("catalog config does not exist".into());
            }
            return Ok(json!({"rows": []}));
        }
        let config = load(&path, true)?;
        let catalog_path = StatePaths::from_config(&path, &config).sqlite_path;
        if !catalog_path.exists() {
            if cli.config.is_some() || tape.is_some() {
                return Err("catalog does not exist".into());
            }
            return Ok(json!({"rows": []}));
        }
        let (rows, fences) = read_catalog_display(&catalog_path, errors, || {
            let index =
                remanence_state::CatalogIndex::open_inventory(&catalog_path).map_err(err)?;
            let fences = index.list_active_tape_io_fences().map_err(err)?;
            let rows = index
                .list_committed_paths(tape.as_deref(), path_prefix.as_deref())
                .map_err(err)?;
            Ok((rows, fences))
        })?;
        output.active_fences = fences.iter().map(crate::tape_io_fence_json).collect();
        if !output.active_fences.is_empty() {
            writeln!(errors, "warning: active tape-I/O fences ({}); catalog reads remain available; inspect rem tape quarantine list", output.active_fences.len()).map_err(err)?;
        }
        return Ok(json!({"rows": rows}));
    }
    let path = config_path(cli, env)?;
    let config = load(
        &path,
        cli.allow_untrusted_state_volume
            || matches!(
                cli.command,
                Command::Doctor | Command::Drives | Command::Diagnostics { .. }
            ),
    )?;
    if let Command::Pool {
        command: Pool::New {
            name,
            scheme,
            block_size,
        },
    } = &cli.command
    {
        if cli.device.is_some() {
            return Err("pool new does not take --device".into());
        }
        if name.trim() != name || name.is_empty() {
            return Err("pool name must be non-empty and trimmed".into());
        }
        let _state =
            StateHandle::open_with_config(StatePaths::from_config(&path, &config), config.clone())
                .map_err(err)?;
        if config.tape_pools.iter().any(|p| p.id == *name) {
            return Err(format!("pool {name} already exists"));
        }
        if let Some(s) = scheme {
            remanence_parity::parse_parity_arg(s).map_err(err)?;
        }
        let mut doc: toml::Value =
            toml::from_str(&fs::read_to_string(&path).map_err(err)?).map_err(err)?;
        let mut pool = toml::map::Map::new();
        pool.insert("id".into(), toml::Value::String(name.clone()));
        pool.insert(
            "scheme".into(),
            toml::Value::String(scheme.clone().unwrap_or_else(|| HOME_POOL_SCHEME.into())),
        );
        if let Some(n) = block_size {
            pool.insert("block_size".into(), toml::Value::Integer((*n).into()));
        }
        let pools = doc
            .as_table_mut()
            .ok_or("invalid config table")?
            .entry("tape_pools")
            .or_insert(toml::Value::Array(vec![]))
            .as_array_mut()
            .ok_or("invalid pools")?;
        pools.push(toml::Value::Table(pool));
        let mut validation = doc.clone();
        if cli.allow_untrusted_state_volume {
            validation
                .get_mut("journal")
                .and_then(toml::Value::as_table_mut)
                .ok_or("missing journal config")?
                .insert("require_trusted_volume".into(), toml::Value::Boolean(false));
        }
        remanence_state::parse_config_toml(&toml::to_string(&validation).map_err(err)?)
            .map_err(err)?;
        let mut temp =
            tempfile::NamedTempFile::new_in(path.parent().ok_or("config has no parent")?)
                .map_err(err)?;
        temp.write_all(toml::to_string(&doc).map_err(err)?.as_bytes())
            .map_err(err)?;
        temp.as_file().sync_all().map_err(err)?;
        temp.persist(&path).map_err(err)?;
        fs::File::open(path.parent().unwrap())
            .and_then(|f| f.sync_all())
            .map_err(err)?;
        return Ok(json!({"pool":name,"config":path}));
    }
    let catalog_path = StatePaths::from_config(&path, &config).sqlite_path;
    let discovery_only = matches!(
        cli.command,
        Command::Doctor | Command::Drives | Command::Diagnostics { .. }
    );
    let mut catalog_error = None;
    if discovery_only && catalog_path.exists() {
        let fences = read_catalog_display(&catalog_path, errors, || {
            remanence_state::CatalogIndex::open_inventory(&catalog_path)
                .map_err(err)
                .and_then(|catalog| {
                    remanence_api::standalone_fence_policy(&catalog, None, None, false).map_err(err)
                })
        });
        match fences {
            Ok(fences) => {
                output.active_fences = fences.iter().map(crate::tape_io_fence_json).collect()
            }
            Err(error) => {
                writeln!(errors, "warning: cannot check catalog fences: {error}").map_err(err)?;
                catalog_error = Some(error);
            }
        }
    }
    let mut state = if discovery_only {
        None
    } else {
        let mut state =
            StateHandle::open_with_config(StatePaths::from_config(&path, &config), config.clone())
                .map_err(err)?;
        state.replay_audit_projection().map_err(err)?;
        output.active_fences =
            remanence_api::standalone_fence_policy(state.catalog_index(), None, None, false)
                .map_err(err)?
                .iter()
                .map(crate::tape_io_fence_json)
                .collect();
        Some(state)
    };
    if !output.active_fences.is_empty() {
        writeln!(errors, "warning: active tape-I/O fences ({}); reads remain available; inspect rem tape quarantine list", output.active_fences.len()).map_err(err)?;
    }
    if let Command::Tape {
        command: Tape::Quarantine { command },
    } = &cli.command
    {
        let state = state.as_mut().expect("quarantine state");
        return match command {
            Quarantine::List => {
                let ops = state
                    .catalog_index()
                    .list_active_media_readiness_operations(None)
                    .map_err(err)?;
                Ok(
                    json!({"active_fences":output.active_fences,"media_readiness":ops.iter().map(crate::media_readiness_record_json).collect::<Vec<_>>()}),
                )
            }
            Quarantine::Release { id, ack } => {
                if ack.trim().is_empty() {
                    return Err("--ack must be non-empty".into());
                }
                let fences = state
                    .catalog_index()
                    .list_active_tape_io_fences()
                    .map_err(err)?;
                if fences.iter().any(|f| {
                    f.quarantine_id == *id && !remanence_api::standalone_fence_releasable(f)
                }) {
                    return Err("fence raised by a library daemon: release it on the host that owns it with rem --config <the library config> tape quarantine release ID --after-settled-inventory --ack TEXT".into());
                }
                let released = state
                    .release_tape_io_fence(id, ack.trim())
                    .map_err(err)?
                    .ok_or("no active tape-I/O fence with this ID")?;
                output.active_fences = remanence_api::standalone_fence_policy(
                    state.catalog_index(),
                    None,
                    None,
                    false,
                )
                .map_err(err)?
                .iter()
                .map(crate::tape_io_fence_json)
                .collect();
                Ok(crate::tape_io_fence_json(&released))
            }
        };
    }
    if let Command::Tape {
        command:
            Tape::AssignPool {
                tape: Some(uuid),
                pool,
                ..
            },
    } = &cli.command
    {
        if cli.device.is_some() {
            return Err("assign-pool requires exactly one of --tape or --device".into());
        }
        assign(
            state.as_mut().expect("assignment state"),
            *uuid.as_bytes(),
            pool.as_deref(),
        )?;
        return Ok(json!({"tape":uuid,"pool":pool}));
    }
    let enumeration = enumeration().map_err(err)?;
    let mut report = match remanence_library::discover_enumerated_with(enumeration, &mut *factory) {
        Ok(r) => r,
        Err(remanence_library::DiscoveryError::NoLibraries { warnings })
            if matches!(
                cli.command,
                Command::Doctor | Command::Drives | Command::Diagnostics { .. }
            ) =>
        {
            let incomplete_evidence: Vec<_> = warnings
                .iter()
                .filter(|warning| warning.is_incomplete_evidence())
                .cloned()
                .collect();
            DiscoveryReport {
                libraries: vec![],
                unattached_drives: vec![],
                complete: incomplete_evidence.is_empty(),
                incomplete_evidence,
                warnings,
            }
        }
        Err(e) => return Err(err(e)),
    };
    for drive in &mut report.unattached_drives {
        if config
            .standalone_drives
            .iter()
            .any(|p| Some(p.serial.as_str()) == drive.serial.as_deref())
        {
            drive.ownership = remanence_library::DriveOwnership::Standalone;
        }
    }
    *observed = remanence_api::observed_endpoint_json(&report);
    for warning in &report.warnings {
        if let Some(message) = remanence_library::drive_mapping_message(warning)
            .or_else(|| warning.informational_message())
        {
            let _ = writeln!(errors, "{message}");
        }
    }
    match &cli.command {
        Command::Doctor => {
            let mut result = doctor(env, &config, &report);
            result["catalog_fence_check"] =
                json!({"ok": catalog_error.is_none(), "error": catalog_error});
            return Ok(result);
        }
        Command::Drives => {
            return Ok(
                json!({"endpoints":observed,"complete":report.complete,"warnings":report.warnings.iter().map(|w| w.informational_message().unwrap_or_else(|| format!("{w:?}"))).collect::<Vec<_>>()}),
            )
        }
        Command::Diagnostics {
            command: Diagnostics::Capture { out },
        } => return diagnostics(cli, &report, factory, out),
        _ => {}
    }
    let mut state = state.expect("device state");
    let device = cli
        .device
        .as_ref()
        .ok_or("this command requires --device PATH and --allow-device <serial|PATH>")?;
    let drive = report
        .unattached_drives
        .iter()
        .find(|d| d.sg_path == *device)
        .ok_or("device is not an unattached drive; library operations go through the daemon")?;
    let pinned = config
        .standalone_drives
        .iter()
        .any(|p| Some(p.serial.as_str()) == drive.serial.as_deref());
    if drive.ownership == remanence_library::DriveOwnership::Unknown && !pinned {
        if !cli.acknowledge_unknown_ownership {
            return Err(
                "unattached drive, ownership unknown; requires --acknowledge-unknown-ownership"
                    .into(),
            );
        }
        if !read_only(&cli.command) {
            return Err("ownership unknown: writes require complete discovery or a configured standalone serial".into());
        }
    }
    if !read_only(&cli.command)
        && report.libraries.iter().any(|l| {
            l.drive_bays
                .iter()
                .any(|b| b.installed.as_ref().is_none_or(|d| d.sg_path.is_none()))
        })
    {
        return Err("unresolved changer bays prohibit device writes".into());
    }
    let authorization = StaticAllowlist::new(cli.allow_device.clone());
    if !authorization.allows_device(device, drive.serial.as_deref()) {
        return Err("--allow-device does not authorize this discovered drive".into());
    }
    // Bind path authorization to the serial observed during ownership discovery.
    // A reattached device must not inherit the previous drive's ownership proof.
    let mut policy = if let Some(serial) = &drive.serial {
        StaticAllowlist::new([serial.clone()])
    } else {
        authorization
    };
    if cli.allow_unserialized_device {
        policy = policy.with_unserialized_device_allowed(device)
    }
    let transport = factory(device).map_err(err)?;
    if matches!(
        cli.command,
        Command::Tape {
            command: Tape::Release
        }
    ) {
        OperatorMount::release_with_transport(device, &policy, transport, &mut state, &env.locks)
            .map_err(err)?;
        return Ok(json!({"released":device}));
    }
    let mut backend =
        OperatorMount::with_transport(device, &policy, transport, &mut state, &env.locks)
            .map_err(err)?;
    let previous_fence_count = output.active_fences.len();
    output.active_fences =
        remanence_api::standalone_fence_policy(backend.catalog_index(), None, None, false)
            .map_err(err)?
            .iter()
            .map(crate::tape_io_fence_json)
            .collect();
    if previous_fence_count != output.active_fences.len() && !output.active_fences.is_empty() {
        writeln!(errors, "warning: active tape-I/O fences ({}); reads remain available; inspect rem tape quarantine list", output.active_fences.len()).map_err(err)?;
    }
    if let Command::Tape {
        command:
            Tape::Init {
                label,
                pool,
                generation,
                block_size,
                clobber_data,
                confirm_clobber,
            },
    } = &cli.command
    {
        let result = backend
            .initialize(
                cli.reserve,
                |drive, state| {
                    prepare_init(
                        drive,
                        state,
                        label,
                        pool.as_deref().unwrap_or("default"),
                        generation.as_deref(),
                        *block_size,
                        (
                            *clobber_data,
                            confirm_clobber.as_deref(),
                            env.test_input,
                            errors,
                        ),
                    )
                },
                complete_init,
            )
            .map_err(err)?;
        backend.finish().map_err(err)?;
        return Ok(result);
    }

    let expected = match &cli.command {
        Command::Tape {
            command: Tape::Finalize { tape, .. },
        } => Some(*tape.as_bytes()),
        _ => None,
    };
    let mut mounted = backend
        .mount(MountTarget::Device {
            path: device.clone(),
            expected_uuid: expected,
        })
        .map_err(err)?;
    if cli.reserve {
        backend.reserve(&mut mounted).map_err(err)?
    }
    let uuid = mounted.tape_uuid;
    let result = match &cli.command {
        Command::Put {
            paths,
            pool,
            recipient,
            allow_plaintext_copy,
            tape: guard,
        } => {
            backend.reconcile_checkpoints().map_err(err)?;
            let tape = backend
                .catalog_index()
                .get_tape(&uuid)
                .map_err(err)?
                .ok_or("loaded tape is not catalogued; use tape adopt")?;
            if let Some(guard) = guard {
                if !Uuid::parse_str(guard)
                    .map(|u| *u.as_bytes() == uuid)
                    .unwrap_or(false)
                    && tape.voltag.as_deref() != Some(guard.as_str())
                {
                    return Err(format!(
                        "requested tape {guard} differs from loaded tape {} ({})",
                        tape.voltag.as_deref().unwrap_or("unlabelled"),
                        Uuid::from_bytes(uuid)
                    ));
                }
            }
            let loaded = tape
                .pool_id
                .ok_or("loaded tape has no pool; use tape assign-pool")?;
            if let Some(p) = pool {
                if p != &loaded {
                    return Err(format!(
                        "requested pool {p} differs from loaded tape pool {loaded}"
                    ));
                }
            }
            let representation = if recipient.is_empty() {
                remanence_api::PoolWriteRepresentation::Plaintext
            } else {
                remanence_api::PoolWriteRepresentation::Encrypted {
                    recipients: crate::pool_ops::read_recipient_files(recipient)?,
                }
            };
            let mut objects = Vec::new();
            let mut failed = None;
            for p in paths {
                let archive = p
                    .file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| p.clone());
                let result = collect_files(p, archive, &mut |source, archive_path, kind| {
                    if let Some(kind) = kind {
                        return output.receipt(
                            json!({"event":"skipped_unsupported","path":archive_path.to_string_lossy(),"kind":kind}),
                        );
                    }
                    let request = remanence_api::WriteObjectToPoolRequest {
                        pool_id: loaded.clone(),
                        caller_object_id: format!("put:{}", Uuid::new_v4()),
                        archive_path: archive_path.clone(),
                        source: remanence_api::WriteObjectSource::Path(source.into()),
                        expected_content_sha256: None,
                        expected_object_id: None,
                        representation: representation.clone(),
                        input_kind: remanence_api::WriteObjectInputKind::LogicalFile,
                    };
                    let outcome = backend.put_path_version_with_policy(
                        &mut mounted,
                        request,
                        *allow_plaintext_copy,
                    );
                    if let Err(remanence_api::MountError::PlaintextCopy { newest_version }) =
                        &outcome
                    {
                        return output.receipt(json!({"event":"refused","reason":"would_store_plaintext_copy_of_encrypted","path":archive_path.to_string_lossy(),"newest_version":newest_version}));
                    }
                    match outcome.map_err(|error| {
                        output.tape_continuation = tape_continuation(&error);
                        format!("{}: {error}", source.display())
                    })? {
                        Ok((result, reason)) => {
                            let mut locator = Vec::new();
                            crate::pool_ops::print_locator_json_with_recipients(
                                &result.object,
                                &loaded,
                                None,
                                &mut locator,
                            );
                            let locator: Value = serde_json::from_slice(&locator).map_err(err)?;
                            output.receipt(json!({"event":"written","reason":reason,"path":archive_path.to_string_lossy(),"object_id":Uuid::from_bytes(result.object.object_id),"tape_uuid":Uuid::from_bytes(uuid),"label":tape.voltag,"bytes":result.object.logical_size_bytes}))?;
                            objects.push(locator);
                        }
                        Err(version) => {
                            let (copy, label) = version
                                .copies
                                .first()
                                .ok_or("committed version has no copy")?;
                            output.receipt(json!({"event":"already_stored","path":archive_path.to_string_lossy(),"object_id":version.object_id,"tape_uuid":Uuid::from_slice(copy).map_err(err)?,"label":label}))?;
                        }
                    }
                    Ok(())
                });
                if let Err(failure) = result {
                    failed = Some(failure);
                    break;
                }
            }
            output.finish(failed.as_ref(), &loaded)?;
            match failed {
                Some((path, error)) => Err(format!("{}: {error}", path.display())),
                None if output.receipts.iter().any(|r| r["event"] == "refused") => {
                    Err("one or more files refused".into())
                }
                None => Ok(
                    json!({"tape":Uuid::from_bytes(uuid),"pool":loaded,"objects":objects,"receipts":output.receipts,"summary":output.summary}),
                ),
            }
        }
        Command::Get {
            object,
            version,
            out,
            private_key,
        } => {
            backend.reconcile_checkpoints().map_err(err)?;
            let normalized;
            let object = if Uuid::parse_str(object).is_ok() {
                object.as_str()
            } else {
                normalized = remanence_stream::normalize_archive_path(Path::new(
                    object.trim_start_matches('/'),
                ))
                .map_err(err)?;
                &normalized
            };
            let object_id = if let Ok(id) = Uuid::parse_str(object) {
                if version.is_some() {
                    return Err("--version requires an archive path selector".into());
                }
                id.to_string()
            } else {
                let versions = backend
                    .catalog_index()
                    .list_committed_versions_by_path_on_tape(object, &uuid)
                    .map_err(err)?;
                let selected = versions
                    .iter()
                    .find(|v| version.is_none_or(|id| v.object_id == id.to_string()));
                if let Some(selected) = selected {
                    selected.object_id.clone()
                } else {
                    let elsewhere = backend
                        .catalog_index()
                        .list_committed_versions_by_path_any_pool(object)
                        .map_err(err)?;
                    let mut locations: Vec<_> = elsewhere
                        .iter()
                        .filter(|v| version.is_none_or(|id| v.object_id == id.to_string()))
                        .flat_map(|v| &v.copies)
                        .map(|(id, label)| {
                            Ok(format!(
                                "{} ({})",
                                label.as_deref().unwrap_or("unlabelled"),
                                Uuid::from_slice(id).map_err(err)?
                            ))
                        })
                        .collect::<Result<_, String>>()?;
                    locations.sort();
                    locations.dedup();
                    if locations.is_empty() {
                        return Err(format!(
                            "{object}: requested version is not catalogued for this path"
                        ));
                    }
                    return Err(format!(
                        "{object} is not on this tape; it is on {}",
                        locations.join(", ")
                    ));
                }
            };
            let record = backend
                .catalog_index()
                .get_native_object(&object_id)
                .map_err(err)?
                .ok_or("object is not catalogued")?;
            let copy = record
                .copies
                .iter()
                .find(|c| c.tape_uuid == uuid && c.status == "committed")
                .ok_or_else(|| {
                    let locations: Vec<_> = record
                        .copies
                        .iter()
                        .filter(|c| c.status == "committed")
                        .map(|c| {
                            let label = backend
                                .catalog_index()
                                .get_tape(&c.tape_uuid)
                                .ok()
                                .flatten()
                                .and_then(|t| t.voltag)
                                .unwrap_or_else(|| "unlabelled".into());
                            format!(
                                "{label} ({})",
                                Uuid::from_slice(&c.tape_uuid)
                                    .map(|u| u.to_string())
                                    .unwrap_or_default()
                            )
                        })
                        .collect();
                    format!(
                        "{object} is not on this tape; it is on {}",
                        locations.join(", ")
                    )
                })?;
            let locator=json!({"object_id":record.object_id,"caller_object_id":record.caller_object_id,"body_format":record.body_format,"pool_id":copy.pool_id,"tape_uuid":crate::bytes_to_hex(&uuid),"tape_file_number":copy.tape_file_number.to_string(),"first_body_lba":copy.first_body_lba.to_string(),"content_sha256":crate::bytes_to_hex(record.content_hash.as_deref().ok_or("object lacks content hash")?)}).to_string();
            // Resolve the caller's destination once; member containment still runs
            // through the shared restore sink beneath this canonical root.
            fs::create_dir_all(out).map_err(err)?;
            let destination = fs::canonicalize(out).map_err(err)?;
            let result = crate::pool_ops::restore_mounted_tape_object(
                mounted,
                backend.catalog_index(),
                &locator,
                private_key.as_deref(),
                &destination,
            );
            backend.finish().map_err(err)?;
            return result;
        }
        Command::Tape { command } => {
            match command {
                Tape::Identify => {
                    let mut facts = terminal_summary(&mut mounted.drive, false)?;
                    facts["uuid"] = json!(Uuid::from_bytes(uuid));
                    facts["tape_uuid"] = json!(Uuid::from_bytes(uuid));
                    facts["label"] = json!(mounted.label.as_ref().map(|p| &p.0));
                    facts["generation"] = json!(mounted.generation.map(|g| g.0.media_code()));
                    let tape = backend.catalog_index().get_tape(&uuid).map_err(err)?;
                    if let Some(finalization) =
                        tape.as_ref().and_then(|t| t.terminal_finalization.as_ref())
                    {
                        facts["physical_status"] = facts["status"].clone();
                        facts["finalization_outcome"] = json!(finalization.outcome.as_str());
                        facts["finalized"] = json!(matches!(
                            finalization.outcome,
                            remanence_state::TerminalFinalizationOutcome::Finalized
                                | remanence_state::TerminalFinalizationOutcome::FinalizedDegraded
                        ));
                        if facts["finalized"] == true && facts["status"] == "not_finalized" {
                            facts["status"] = json!("terminal_index_unusable");
                        }
                    }
                    facts["pool"] = json!(tape.as_ref().and_then(|t| t.pool_id.as_ref()));
                    if facts["object_count"].is_null() && tape.is_some() {
                        let files = backend
                            .catalog_index()
                            .list_tape_files(&uuid)
                            .map_err(err)?;
                        let objects: Vec<_> =
                            files.iter().filter(|f| f.object_id.is_some()).collect();
                        facts["object_count"] = json!(objects.len());
                        facts["bytes"] = json!(objects
                            .iter()
                            .map(|f| f.block_count.saturating_mul(
                                tape.as_ref().and_then(|t| t.block_size).unwrap_or(0)
                            ))
                            .sum::<u64>());
                        facts["inventory_source"] = json!("catalog");
                    }
                    Ok(facts)
                }
                Tape::Verify { full } => {
                    if *full {
                        let bootstrap = remanence_parity::bootstrap::discover_bootstrap(
                            &mut remanence_parity::DriveHandleRawSource::new(&mut mounted.drive),
                            None,
                        )
                        .map_err(err)?;
                        let result = remanence_parity::verify_terminal_index_full(
                            &mut remanence_parity::DriveHandleRawSource::new(&mut mounted.drive),
                            &uuid,
                            bootstrap.block_size_bytes,
                        )
                        .map_err(err)?;
                        let complete = result.is_complete_tape();
                        let facts = match result {
                            remanence_parity::TerminalIndexVerificationOutcome::VerifiedComplete(v) | remanence_parity::TerminalIndexVerificationOutcome::VerifiedDegraded(v) => json!({"verified_complete": complete, "finalized": true, "object_count": v.selected_payload.object_row_count, "verified_prefix_bytes": v.verified_prefix_record_count.saturating_mul(u64::from(bootstrap.block_size_bytes)), "edition_digest": crate::bytes_to_hex(&v.edition.edition_digest)}),
                            remanence_parity::TerminalIndexVerificationOutcome::RecoveryRequired(r) => json!({"verified_complete": false, "status": "recovery_required", "detail": r.detail}),
                        };
                        if complete {
                            Ok(facts)
                        } else {
                            Err(format!("full verification incomplete: {facts}"))
                        }
                    } else {
                        terminal_summary(&mut mounted.drive, true)
                    }
                }
                Tape::Restore {
                    out,
                    private_key,
                    scan,
                    list,
                    path,
                } => {
                    let key = private_key
                        .as_deref()
                        .map(crate::pool_ops::read_private_key_file)
                        .transpose()?;
                    let report = remanence_api::restore_catalogless_selected(
                        &mut mounted.drive,
                        out.as_deref(),
                        key.as_ref(),
                        *scan,
                        *list,
                        path,
                    )?;
                    if !*list
                        && report["candidates"].as_array().is_some_and(|items| {
                            items.iter().any(|item| item["status"] != "restored")
                        })
                    {
                        Err(format!("restore incomplete: {report}"))
                    } else {
                        Ok(report)
                    }
                }
                Tape::Adopt { label, pool } => {
                    let observation = mounted
                        .generation
                        .map(|(generation, source)| MediaObservation { generation, source });
                    let endpoint = mounted
                        .endpoint
                        .durable_key()
                        .cloned()
                        .ok_or("adopt requires serialized drive")?;
                    let mut config = mounted.drive.read_config().map_err(err)?;
                    config.block_size = BlockSize::Variable;
                    config.compression = false;
                    mounted.drive.write_config(config).map_err(err)?;
                    let projection = remanence_api::classify_bootstrap_adoption_from_source(
                        &mut DriveHandleSource(&mut mounted.drive),
                    );
                    let payload = projection.payload.ok_or("invalid BOT during adoption")?;
                    let tail=match projection.tail {
                    remanence_api::BootstrapTailClassification::ExactBootstrapFilemarkEod=>remanence_state::BootstrapAdoptionTailEvidence::ExactBootstrapFilemarkEod,
                    remanence_api::BootstrapTailClassification::DataAfterBootstrap=>remanence_state::BootstrapAdoptionTailEvidence::DataAfterBootstrap,
                    remanence_api::BootstrapTailClassification::MissingFilemark=>remanence_state::BootstrapAdoptionTailEvidence::MissingFilemark,
                    remanence_api::BootstrapTailClassification::ExtraFilemark=>remanence_state::BootstrapAdoptionTailEvidence::ExtraFilemark,
                    _=>remanence_state::BootstrapAdoptionTailEvidence::Ambiguous,
                };
                    let serial = match &endpoint {
                        remanence_library::DriveKey::Standalone { drive_serial } => {
                            drive_serial.clone()
                        }
                        _ => unreachable!(),
                    };
                    backend.mutate_catalog(&mounted,|state|{
                    let parity=payload.scheme.as_ref().map(|s|remanence_parity::ParityConfig::Scheme(remanence_parity::ParityScheme{id:remanence_parity::SchemeId::new_owned(s.id.clone()),data_blocks_per_stripe:s.data_blocks_per_stripe,parity_blocks_per_stripe:s.parity_blocks_per_stripe,stripes_per_neighborhood:s.stripes_per_neighborhood})).unwrap_or(remanence_parity::ParityConfig::None);
                    let result=state.adopt_bootstrap_identity(remanence_state::AdoptBootstrapIdentityInput{operation_id:Uuid::new_v4(),tape_uuid:uuid,voltag:label.clone(),block_size:payload.block_size_bytes,parity,label_provenance:LabelProvenance::Operator,observation,pool_id:Some(pool.clone().unwrap_or_else(||"default".into()))},remanence_state::BootstrapAdoptionEvidence{endpoint,library_revision:None,home_slot:None,drive_serial:serial,bootstrap_drive_compression:payload.drive_compression,configured_drive_compression:false,physical_tail:tail}).map_err(err)?;
                    Ok(json!({"adopted":Uuid::from_bytes(uuid),"state":result.state.as_str()}))
                }).map_err(err)
                }
                Tape::AssignPool { pool, .. } => backend
                    .mutate_catalog(&mounted, |state| {
                        assign(state, uuid, pool.as_deref())?;
                        Ok(json!({"tape":Uuid::from_bytes(uuid),"pool":pool}))
                    })
                    .map_err(err),
                Tape::Rebind {
                    label,
                    previous_uuid,
                } => backend
                    .mutate_catalog(&mounted, |state| {
                        state
                            .rebind_tape_label(uuid, label, *previous_uuid.as_bytes())
                            .map_err(err)?;
                        Ok(json!({"label":label}))
                    })
                    .map_err(err),
                Tape::SetGeneration { generation } => backend
                    .mutate_catalog(&mounted, |state| {
                        state
                            .set_tape_generation(
                                uuid,
                                MediaObservation {
                                    generation: parse_generation(generation)?,
                                    source: GenerationSource::Operator,
                                },
                            )
                            .map_err(err)?;
                        Ok(json!({"generation":generation}))
                    })
                    .map_err(err),
                Tape::SupersedeTail {
                    acknowledge_tape,
                    confirm_supersede,
                } => {
                    let discarded = backend.supersede_tail(&mut mounted, acknowledge_tape, |expected, found| {
                        eprintln!("Tail to discard: {}", found.join(", "));
                        let confirmation = match confirm_supersede {
                            Some(value) => value.clone(),
                            None => {
                                use std::io::IsTerminal;
                                if !std::io::stdin().is_terminal() {
                                    return Err(format!("non-interactive supersede requires --confirm-supersede {expected:?}"));
                                }
                                eprint!("Type {expected:?} to discard this tail: ");
                                std::io::stderr().flush().map_err(err)?;
                                let mut line = String::new();
                                std::io::stdin().read_line(&mut line).map_err(err)?;
                                line.trim_end_matches(['\r', '\n']).to_owned()
                            }
                        };
                        if confirmation != expected { return Err("supersede confirmation did not match".into()); }
                        Ok(())
                    }).map_err(err)?;
                    Ok(json!({"discarded_object_ids": discarded}))
                }
                Tape::ImportJournals { from } => {
                    backend.import_journals(&mut mounted, from).map_err(err)?;
                    Ok(json!({"imported_from":from}))
                }
                Tape::Finalize {
                    tape,
                    acknowledge_tape,
                    reason,
                    idempotency_key,
                    expected_pool,
                } => {
                    let result = backend
                        .finalize(
                            &mut mounted,
                            remanence_api::StandaloneFinalizeRequest {
                                tape_uuid: *tape.as_bytes(),
                                acknowledge_tape: *acknowledge_tape.as_bytes(),
                                reason: reason.clone(),
                                idempotency_key: *idempotency_key,
                                expected_pool_id: expected_pool.clone(),
                            },
                        )
                        .map_err(err)?;
                    Ok(
                        json!({"finalized": matches!(result.outcome, remanence_state::TerminalFinalizationOutcome::Finalized | remanence_state::TerminalFinalizationOutcome::FinalizedDegraded), "outcome": result.outcome.as_str(), "operation_id": result.operation_id, "completed_replicas": result.completed_replicas, "edition_digest": crate::bytes_to_hex(&result.edition_digest), "layout_digest": crate::bytes_to_hex(&result.layout_digest)}),
                    )
                }
                _ => unreachable!("pre-mount command"),
            }
        }
        _ => unreachable!("pre-device command"),
    };
    drop(mounted);
    backend.finish().map_err(err)?;
    result
}
/// Only tape-capacity authority may suggest closing a cartridge; host I/O cannot.
fn tape_continuation(error: &remanence_api::MountError) -> bool {
    if matches!(error, remanence_api::MountError::EarlyWarning) {
        return true;
    }
    matches!(error, remanence_api::MountError::PoolWrite(cause) if matches!(cause.as_ref(),
    remanence_api::PoolWriteError::SelectedTapeInsufficientCapacity { .. }
    | remanence_api::PoolWriteError::TerminalCloseRequired { .. }
    | remanence_api::PoolWriteError::Parity(remanence_parity::ParityError::CapacityReserveExceeded {
        cause: remanence_parity::CapacityReserveCause::TapeCapacity, ..
    })))
}

/// Stream receipts before proceeding to another file, retaining them for the envelope.
struct PutOutput<'a> {
    active_fences: Vec<Value>,
    out: &'a mut dyn Write,
    json: bool,
    receipts: Vec<Value>,
    summary: Option<Value>,
    tape_continuation: bool,
}
impl PutOutput<'_> {
    fn receipt(&mut self, receipt: Value) -> Result<(), String> {
        if self.json {
            writeln!(self.out, "{receipt}").map_err(err)?;
        } else {
            writeln!(
                self.out,
                "{:<20} {}{}",
                receipt["event"].as_str().unwrap_or(""),
                receipt["path"].as_str().unwrap_or(""),
                if receipt["event"] == "skipped_unsupported" {
                    format!(" ({})", receipt["kind"].as_str().unwrap_or(""))
                } else if receipt["event"] == "refused" {
                    format!(
                        "  {} (newest version {}); use --allow-plaintext-copy to override",
                        receipt["reason"].as_str().unwrap_or(""),
                        receipt["newest_version"].as_str().unwrap_or("")
                    )
                } else {
                    format!(
                        "  {}  {} ({}){}",
                        receipt["object_id"].as_str().unwrap_or(""),
                        receipt["label"].as_str().unwrap_or("unlabelled"),
                        receipt["tape_uuid"].as_str().unwrap_or(""),
                        receipt
                            .get("bytes")
                            .map(|b| format!("  {b} bytes"))
                            .unwrap_or_default()
                    )
                }
            )
            .map_err(err)?;
        }
        self.out.flush().map_err(err)?;
        self.receipts.push(receipt);
        Ok(())
    }
    fn finish(&mut self, failed: Option<&(PathBuf, String)>, pool: &str) -> Result<(), String> {
        let count = |event| self.receipts.iter().filter(|r| r["event"] == event).count();
        let (written, already_stored, skipped) = (
            count("written"),
            count("already_stored"),
            count("skipped_unsupported"),
        );
        let refused = count("refused");
        let next = (failed.is_some() && self.tape_continuation).then(|| {
            format!("finalize this tape, init the next in pool {pool}, and re-run the same put")
        });
        let summary = json!({"event":"summary","written":written,"already_stored":already_stored,"skipped":skipped,"refused":refused,"failed":failed.map(|(path,error)|json!({"path":path.to_string_lossy(),"error":error})),"next":next});
        if self.json {
            writeln!(self.out, "{summary}").map_err(err)?;
        } else {
            writeln!(
                self.out,
                "Total: {written} written, {already_stored} already_stored, {skipped} skipped, {refused} refused{}",
                failed
                    .map(|(p, e)| format!(
                        "; failed {}: {e}; {}",
                        p.display(),
                        next.as_deref().unwrap_or("")
                    ))
                    .unwrap_or_default()
            )
            .map_err(err)?;
        }
        self.out.flush().map_err(err)?;
        self.summary = Some(summary);
        Ok(())
    }
}

/// Walk deterministically without following links; deliver each decision immediately.
fn collect_files(
    source: &Path,
    archive: PathBuf,
    visit: &mut impl FnMut(&Path, PathBuf, Option<&str>) -> Result<(), String>,
) -> Result<(), (PathBuf, String)> {
    use std::os::unix::fs::FileTypeExt;
    let at = |e| (archive.clone(), err(e));
    let meta = fs::symlink_metadata(source).map_err(at)?;
    if meta.is_dir() {
        let mut entries = fs::read_dir(source)
            .map_err(at)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(at)?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            collect_files(&e.path(), archive.join(e.file_name()), visit)?;
        }
    } else {
        let kind = if meta.is_file() {
            None
        } else if meta.file_type().is_symlink() {
            Some("symlink")
        } else if meta.file_type().is_fifo() {
            Some("fifo")
        } else if meta.file_type().is_socket() {
            Some("socket")
        } else {
            Some("device")
        };
        visit(source, archive.clone(), kind).map_err(|e| (archive, e))?;
    }
    Ok(())
}
fn parse_generation(s: &str) -> Result<remanence_api::LtoGen, String> {
    remanence_api::LtoGen::from_media_code(s)
        .ok_or_else(|| format!("unknown cartridge generation {s}"))
}
/// Read-only initialization planning; the backend installs durable write guards
/// after this returns the planned identity and before complete_init can write.
struct InitPlan {
    uuid: [u8; 16],
    label: String,
    pool: String,
    size: u32,
    parity: remanence_parity::ParityConfig,
    observation: MediaObservation,
    decision: remanence_api::InitDecision,
    clobber_data_confirmed: bool,
    previous_uuid: Option<[u8; 16]>,
}
fn prepare_init(
    drive: &mut DriveHandle,
    state: &mut StateHandle,
    label: &str,
    pool: &str,
    generation: Option<&str>,
    block_size: Option<u32>,
    clobber: (bool, Option<&str>, bool, &mut dyn Write),
) -> Result<([u8; 16], InitPlan), String> {
    let pool_config = state
        .config()
        .tape_pools
        .iter()
        .find(|p| p.id == pool)
        .cloned()
        .ok_or_else(|| format!("unconfigured pool {pool}"))?;
    if label.is_empty() || label.trim() != label {
        return Err("label must be non-empty and trimmed".into());
    }
    let ready = drive.wait_for_media_readiness(
        remanence_library::MediaFamily::Unknown,
        None,
        remanence_library::MediaReadinessWaitOptions {
            wait: true,
            timeout: std::time::Duration::from_secs(9000),
            poll_interval: std::time::Duration::from_secs(30),
        },
        || None,
        |_| Ok(()),
    )?;
    if ready.readiness != remanence_library::MediaReadiness::Ready {
        return Err(format!("media not ready: {:?}", ready.readiness));
    }
    drive.prevent_medium_removal().map_err(err)?;
    let proof = drive.identity_generation();
    drive.require_single_partition().map_err(err)?;
    let mut cfg = drive.read_config().map_err(err)?;
    if cfg.write_protected {
        return Err("tape is write protected".into());
    }
    let hint = generation
        .map(|g| {
            parse_generation(g).map(|generation| MediaObservation {
                generation,
                source: GenerationSource::Operator,
            })
        })
        .transpose()?
        .or_else(|| MediaObservation::label_hint(label));
    let observation = match cfg
        .density_code
        .and_then(remanence_api::lto_generation_from_density)
    {
        Some(g) => Some(MediaObservation::measured(g, hint, generation.is_some())?),
        None => hint,
    }
    .ok_or("unknown generation; specify --generation Lx")?;
    let size = block_size.unwrap_or(u32::try_from(pool_config.block_size_bytes).map_err(err)?);
    crate::recovery_report::parse_block_size(&size.to_string())?;
    if size > cfg.max_block_size_bytes {
        return Err("block size exceeds drive READ BLOCK LIMITS".into());
    }
    cfg.block_size = BlockSize::Variable;
    cfg.compression = false;
    drive.write_config(cfg).map_err(err)?;
    let bot = remanence_api::classify_bot_from_source(&mut DriveHandleSource(drive));
    let loaded_uuid = match &bot.classification {
        remanence_api::BotClassification::OursBootstrap { uuid, .. } => Some(*uuid),
        _ => None,
    };
    remanence_api::standalone_fence_policy(
        state.catalog_index(),
        loaded_uuid,
        drive.endpoint().durable_key(),
        true,
    )
    .map_err(err)?;
    let facts = remanence_api::project_tape_init_catalog_inputs(
        state.catalog_index(),
        label,
        &bot.classification,
        pool,
    )
    .map_err(err)?;
    if let remanence_api::LabelLifecycleState::AssignedTo { uuid, .. } = &facts.barcode_state {
        if loaded_uuid != Some(*uuid) {
            return Err(format!("label {label} belongs to tape {}; use tape rebind --previous-uuid {} or choose a new label", Uuid::from_bytes(*uuid), Uuid::from_bytes(*uuid)));
        }
    }
    let decision = remanence_api::decide_tape_init(
        &bot.classification,
        facts.catalog_row.as_ref(),
        &facts.barcode_state,
        pool,
        bot.physical_data_past_bootstrap,
        &facts.committed_copies,
    );
    let parity = pool_config
        .scheme
        .as_deref()
        .map(|s| resolve_init_scheme(s, size))
        .transpose()
        .map_err(err)?
        .unwrap_or_else(|| {
            remanence_parity::ParityConfig::Scheme(remanence_parity::default_scheme_for_block_size(
                size,
            ))
        });
    let replacement_uuid = *Uuid::new_v4().as_bytes();
    let previous_uuid = loaded_uuid.filter(|_| {
        facts
            .catalog_row
            .as_ref()
            .is_some_and(|row| row.disposition != remanence_api::CatalogRowDisposition::Retired)
    });
    if crate::should_confirm_clobber(clobber.0, &decision) {
        state
            .catalog_index()
            .provision_replacing_media(
                remanence_state::ProvisionTapeInput {
                    tape_uuid: replacement_uuid,
                    voltag: label.into(),
                    block_size: size,
                    parity: parity.clone(),
                    force: true,
                },
                LabelProvenance::Operator,
                Some(observation),
                previous_uuid,
                true,
            )
            .map_err(err)?;
    }
    let clobber_data_confirmed = if crate::should_confirm_clobber(clobber.0, &decision) {
        use std::io::IsTerminal;
        if clobber.2 {
            crate::confirm_clobber_data(
                label,
                &decision,
                clobber.1,
                crate::ClobberInputPolicy::Standalone { is_terminal: false },
                &mut std::io::Cursor::new([]),
                clobber.3,
            )?
        } else {
            crate::confirm_clobber_data(
                label,
                &decision,
                clobber.1,
                crate::ClobberInputPolicy::Standalone {
                    is_terminal: std::io::stdin().is_terminal(),
                },
                &mut std::io::stdin().lock(),
                clobber.3,
            )?
        }
    } else {
        false
    };
    if !clobber_data_confirmed
        && !matches!(
            decision,
            remanence_api::InitDecision::FreshInit | remanence_api::InitDecision::IdempotentNoOp
        )
    {
        use remanence_api::{InitDecision, TapeInitError};
        let reason = match &decision {
            InitDecision::RefuseClobber { reason: TapeInitError::ForeignFormat(name) } =>
                format!("this cartridge holds {name} data at the beginning; re-run with --clobber-data to erase it for Remanence"),
            InitDecision::RefuseClobber { reason: TapeInitError::UnrecognizedData } =>
                "this cartridge holds unrecognized data at the beginning; re-run with --clobber-data to erase it for Remanence".into(),
            InitDecision::RefuseClobber { reason } =>
                format!("Cannot initialize this cartridge: {}; re-run with --clobber-data to erase it for Remanence.", crate::init_error_label(reason)),
            InitDecision::RequireForce { reason }
            | InitDecision::Anomaly { reason } | InitDecision::NeedsExplicitRebuild { reason } =>
                format!("Cannot initialize this cartridge: {}.", crate::init_error_label(reason)),
            _ => unreachable!(),
        };
        return Err(reason);
    }
    let (uuid, size, parity) = crate::planned_init_geometry(
        &bot.classification,
        &decision,
        clobber_data_confirmed,
        size,
        parity,
    );
    let uuid = if clobber_data_confirmed {
        replacement_uuid
    } else {
        uuid
    };
    cfg.block_size = BlockSize::Fixed { size_bytes: size };
    drive.write_config(cfg).map_err(err)?;
    drive.rewind().map_err(err)?;
    drive.bind_identity_generation(proof).map_err(err)?;
    Ok((
        uuid,
        InitPlan {
            uuid,
            label: label.into(),
            pool: pool.into(),
            size,
            parity,
            observation,
            decision,
            clobber_data_confirmed,
            previous_uuid: if clobber_data_confirmed {
                previous_uuid
            } else {
                None
            },
        },
    ))
}
fn resolve_init_scheme(
    scheme: &str,
    size: u32,
) -> Result<remanence_parity::ParityConfig, remanence_parity::ParityError> {
    use remanence_parity::{
        conservative_scheme_for_block_size, default_scheme_for_block_size, parse_parity_arg,
        ParityConfig,
    };
    match scheme.trim() {
        "default" => Ok(ParityConfig::Scheme(default_scheme_for_block_size(size))),
        "conservative" => Ok(ParityConfig::Scheme(conservative_scheme_for_block_size(
            size,
        ))),
        other => parse_parity_arg(other),
    }
}
/// Runs only inside OperatorMount's durable initialization guard.
fn complete_init(
    drive: &mut DriveHandle,
    state: &mut StateHandle,
    plan: InitPlan,
) -> Result<Value, String> {
    let InitPlan {
        uuid,
        label,
        pool,
        size,
        parity,
        observation,
        decision,
        clobber_data_confirmed,
        previous_uuid,
    } = plan;
    let action = remanence_api::maybe_write_tape_init_bootstrap_on_drive(
        drive,
        &decision,
        remanence_api::TapeInitWriteOptions {
            clobber_data_confirmed,
            ..Default::default()
        },
        uuid,
        size,
        parity.clone(),
        &remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION")),
    )
    .map_err(err)?;
    if action == remanence_api::TapeInitWriteAction::Refused {
        return Err("initialization refused by the media write gate; no write attempted".into());
    }
    if action == remanence_api::TapeInitWriteAction::WroteBootstrap {
        let config = state.config().clone();
        crate::provision_initialized_tape_with_media(
            state,
            &config,
            remanence_state::ProvisionTapeInput {
                tape_uuid: uuid,
                voltag: label.clone(),
                block_size: size,
                parity,
                force: clobber_data_confirmed,
            },
            crate::ProvisioningContext {
                provenance: LabelProvenance::Operator,
                observation: Some(observation),
                explicit_pool: Some(&pool),
                previous_uuid,
            },
        )?;
    }
    Ok(
        json!({"tape_uuid":Uuid::from_bytes(uuid),"label":label,"pool":pool,"action":format!("{action:?}")}),
    )
}
fn terminal_summary(drive: &mut DriveHandle, verifying: bool) -> Result<Value, String> {
    let bootstrap = remanence_parity::bootstrap::discover_bootstrap(
        &mut remanence_parity::DriveHandleRawSource::new(drive),
        None,
    )
    .map_err(err)?;
    let mut bytes = 0u64;
    let result = remanence_parity::read_terminal_index_inventory_streamed(
        &mut remanence_parity::DriveHandleRawSource::new(drive),
        &bootstrap.tape_uuid,
        bootstrap.block_size_bytes,
        |event| {
            match event {
                remanence_parity::TerminalInventoryStreamEvent::ReplicaAttemptStarted {
                    ..
                }
                | remanence_parity::TerminalInventoryStreamEvent::ReplicaAttemptRejected {
                    ..
                } => bytes = 0,
                remanence_parity::TerminalInventoryStreamEvent::ObjectRow { row, .. } => {
                    bytes = bytes.saturating_add(
                        row.stored_block_count
                            .saturating_mul(u64::from(bootstrap.block_size_bytes)),
                    )
                }
                _ => {}
            }
            Ok(())
        },
    )
    .map_err(err)?;
    let mut summary = summarize_terminal(result, verifying)?;
    summary["bytes"] = if summary["finalized"] == true {
        json!(bytes)
    } else {
        Value::Null
    };
    Ok(summary)
}
/// Distinguish absent terminal authority from a discovered but unusable index.
fn terminal_recovery_status(
    reason: &remanence_parity::BotStructuralRecoveryReason,
) -> &'static str {
    match reason {
        remanence_parity::BotStructuralRecoveryReason::NoUsableTerminalLayout => "not_finalized",
        remanence_parity::BotStructuralRecoveryReason::AllMembersInvalid => {
            "terminal_index_unusable"
        }
    }
}
fn summarize_terminal(
    result: remanence_parity::TerminalInventoryOutcome,
    verifying: bool,
) -> Result<Value, String> {
    match result {
        remanence_parity::TerminalInventoryOutcome::BotStructuralRecoveryRequired(ref recovery)
            if verifying =>
        {
            Err(format!(
                "verification failed: no usable terminal index; {}",
                terminal_recovery_status(&recovery.reason)
            ))
        }
        remanence_parity::TerminalInventoryOutcome::Inventory(inventory) => Ok(
            json!({"finalized": true, "object_count": inventory.payload.object_row_count, "replica": inventory.selected_replica_ordinal, "edition_digest": crate::bytes_to_hex(&inventory.edition.edition_digest), "status": if verifying { "verified" } else { "identified" }}),
        ),
        remanence_parity::TerminalInventoryOutcome::BotStructuralRecoveryRequired(recovery) => Ok(
            json!({"finalized": false, "object_count": null, "status": terminal_recovery_status(&recovery.reason)}),
        ),
    }
}
/// Report effective group membership and the executable's file capability, not a different binary.
fn doctor(
    env: &Environment,
    config: &remanence_state::RemConfig,
    report: &DiscoveryReport,
) -> Value {
    let groups = std::process::Command::new("id").arg("-Gn").output();
    let tape_group = groups
        .as_ref()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .any(|s| s == "tape")
        });
    let binary = std::env::current_exe().ok();
    let capability = binary
        .as_ref()
        .and_then(|p| xattr::get(p, "security.capability").ok().flatten());
    // Linux vfs_cap_data: magic then permitted/inheritable words; CAP_SYS_RAWIO=17.
    let cap_sys_rawio = capability.as_ref().is_some_and(|b| {
        b.len() >= 12
            && u32::from_le_bytes(b[0..4].try_into().unwrap()) & 1 != 0
            && u32::from_le_bytes(b[4..8].try_into().unwrap()) & (1 << 17) != 0
    });
    let lock_writable = fs::create_dir_all(&env.locks)
        .and_then(|_| tempfile::NamedTempFile::new_in(&env.locks))
        .map(|_| true)
        .map_err(err);
    json!({"tape_group":tape_group,"cap_sys_rawio_on_this_binary":cap_sys_rawio,"binary":binary,"state_dir":config.daemon.state_dir,"lock_dir":"/run/lock/rem","lock_dir_writable":lock_writable.is_ok(),"lock_error":lock_writable.err(),"drives":remanence_api::observed_endpoint_json(report),"fixes":{"tape_group":"add this user to the tape group and log in again","capability":"setcap cap_sys_rawio+ep on the rem executable shown above","lock_dir":"create /run/lock/rem with permissions for the tape group"}})
}
/// Fixed command allowlist: no READ, SPACE, LOCATE, or data-out commands occur here.
fn diagnostics<F>(
    cli: &Cli,
    report: &DiscoveryReport,
    factory: &mut F,
    out: &Path,
) -> Result<Value, String>
where
    F: FnMut(&Path) -> Result<Box<dyn SgTransport>, IoErrorKind>,
{
    let temp = tempfile::tempdir().map_err(err)?;
    let mut devices: Vec<(PathBuf, bool, Option<String>)> = report
        .unattached_drives
        .iter()
        .map(|d| (d.sg_path.clone(), false, d.serial.clone()))
        .collect();
    for library in &report.libraries {
        devices.push((
            library.changer_sg.clone(),
            true,
            Some(library.serial.as_str().into()),
        ));
        for bay in &library.drive_bays {
            if let Some(p) = bay.installed.as_ref().and_then(|d| d.sg_path.as_ref()) {
                devices.push((
                    p.clone(),
                    false,
                    bay.installed.as_ref().map(|d| d.serial.as_str().into()),
                ));
            }
        }
    }
    if let Some(path) = &cli.device {
        devices.retain(|(p, _, _)| p == path);
        if devices.is_empty() {
            return Err("diagnostic device was not discovered".into());
        }
    }
    let policy = StaticAllowlist::new(cli.allow_device.clone());
    let mut records = vec![];
    for (i, (path, changer, serial)) in devices.iter().enumerate() {
        if cli.device.is_some() && !policy.allows_device(path, serial.as_deref()) {
            return Err("diagnostic device requires matching --allow-device".into());
        }
        let mut transport = factory(path).map_err(err)?;
        let mut commands = vec![
            ("inquiry", vec![0x12, 0, 0, 0, 255, 0], 255),
            ("vpd80", vec![0x12, 1, 0x80, 0, 255, 0], 255),
            ("vpd83", vec![0x12, 1, 0x83, 0, 255, 0], 255),
        ];
        if *changer {
            commands.push((
                "res",
                vec![0xb8, 0x10, 0, 0, 0xff, 0xff, 1, 0, 0xff, 0xff, 0, 0],
                65535,
            ));
        } else {
            commands.push(("mode-sense", vec![0x1a, 0, 0x3f, 0, 255, 0], 255));
            for (name, page) in [
                ("log-supported", 0),
                ("log-write-errors", 2),
                ("log-read-errors", 3),
                ("log-tapealert", 0x2e),
            ] {
                commands.push((
                    name,
                    vec![0x4d, 0, 0x40 | page, 0, 0, 0, 0, 0xff, 0xff, 0],
                    65535,
                ));
            }
        }
        for (name, cdb, size) in commands {
            let mut buf = vec![0; size];
            match transport.execute_in(&cdb, &mut buf) {
                Ok(result) => {
                    let filename = format!("device-{i}-{name}.bin");
                    let count = (result.bytes_transferred as usize).min(buf.len());
                    fs::write(temp.path().join(&filename), &buf[..count]).map_err(err)?;
                    records
                        .push(json!({"device":path,"command":name,"file":filename,"bytes":count}));
                }
                Err(e) => records.push(json!({"device":path,"command":name,"error":e.to_string()})),
            }
        }
    }
    for (name, program, args) in [
        ("lsscsi.txt", "lsscsi", vec!["-g"]),
        ("kernel.txt", "uname", vec!["-a"]),
        ("hba.txt", "lspci", vec!["-nnk"]),
    ] {
        let output = std::process::Command::new(program).args(args).output();
        let text = match output {
            Ok(o) => format!(
                "exit: {}\n{}\n{}",
                o.status,
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            ),
            Err(e) => format!("unavailable: {e}"),
        };
        fs::write(temp.path().join(name), text).map_err(err)?;
    }
    fs::write(temp.path().join("manifest.json"),serde_json::to_vec_pretty(&json!({"commands":records,"observed_endpoints":remanence_api::observed_endpoint_json(report),"tape_data_captured":false})).map_err(err)?).map_err(err)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out)
        .map_err(err)?;
    let status = std::process::Command::new("tar")
        .arg("-cf")
        .arg("-")
        .arg("-C")
        .arg(temp.path())
        .arg(".")
        .stdout(file)
        .status()
        .map_err(err)?;
    if !status.success() {
        return Err(format!("tar failed: {status}"));
    }
    Ok(json!({"tarball":out,"tape_data_captured":false}))
}

/// Retry display-only reads once if the database or WAL changed, including a
/// writer appearing during an immutable read. Failed reads are retried too.
fn read_catalog_display<T>(
    path: &Path,
    errors: &mut dyn Write,
    mut read: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    use std::os::unix::fs::MetadataExt;
    let stamp = || {
        let mut wal = path.as_os_str().to_os_string();
        wal.push("-wal");
        [path, Path::new(&wal)]
            .map(|p| match fs::metadata(p) {
                Ok(m) => Ok(Some((m.ino(), m.len(), m.mtime(), m.mtime_nsec()))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(err(e)),
            })
            .into_iter()
            .collect::<Result<Vec<_>, String>>()
    };
    for attempt in 0..2 {
        let before = stamp()?;
        let result = read();
        if before == stamp()? {
            return result;
        }
        if attempt == 1 {
            writeln!(
                errors,
                "warning: catalog changed during the read; listing may be stale"
            )
            .map_err(err)?;
            return result;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    /// Stamp changes retry once, then retain the result with a visible warning.
    #[test]
    fn catalog_display_retries_and_warns() {
        use super::*;
        for changes in [0, 1, 2] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("catalog");
            fs::write(&path, b"").unwrap();
            let mut calls = 0;
            let mut warnings = Vec::new();
            let value = read_catalog_display(&path, &mut warnings, || {
                calls += 1;
                if calls <= changes {
                    fs::write(&path, vec![0; calls]).unwrap();
                }
                Ok(calls)
            })
            .unwrap();
            assert_eq!(value, if changes == 0 { 1 } else { 2 });
            assert_eq!(!warnings.is_empty(), changes == 2);
        }
    }

    /// Reading library quarantine state never generates a replacement home config.
    #[test]
    fn quarantine_fallback_reports_path_without_creating_config() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let env = Environment {
            xdg: root.path().join("state"),
            system_config: root.path().join("system.toml"),
            locks: root.path().join("locks"),
            test_input: true,
        };
        let mut errors = Vec::new();
        assert!(local_quarantine_config_in(&env.system_config, &env, &mut errors).is_err());
        assert!(!env.xdg.exists());
        assert!(String::from_utf8(errors).unwrap().contains("using config"));
        fs::create_dir_all(env.xdg.join("rem")).unwrap();
        let home = env.xdg.join("rem/config.toml");
        fs::write(&home, "").unwrap();
        assert_eq!(
            local_quarantine_config_in(&env.system_config, &env, &mut Vec::new()).unwrap(),
            home
        );
    }

    /// Shared quarantine routing follows home-mode configuration, never reason text.
    #[test]
    fn quarantine_routing_requires_home_drive_and_no_library_config() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let env = Environment {
            xdg: root.path().into(),
            system_config: root.path().join("system.toml"),
            locks: root.path().join("locks"),
            test_input: true,
        };
        fs::create_dir(root.path().join("rem")).unwrap();
        let args = [
            "rem",
            "--allow-untrusted-state-volume",
            "tape",
            "quarantine",
            "list",
        ]
        .map(OsString::from);
        assert!(!selected_in(&args, &env));
        let cli = Cli::try_parse_from(["rem", "doctor"]).unwrap();
        let path = config_path(&cli, &env).unwrap();
        let text = fs::read_to_string(&path).unwrap() + "\n[home]\ndrive_serial = \"DRIVE\"\n";
        fs::write(&path, text).unwrap();
        let config = load(&path, true).unwrap();
        assert_eq!(
            config.home.as_ref().unwrap().drive_serial.as_deref(),
            Some("DRIVE")
        );
        let mut state =
            StateHandle::open_with_config(StatePaths::from_config(&path, &config), config).unwrap();
        let fence = state
            .catalog_index()
            .record_tape_io_fence(remanence_state::TapeIoFenceInput {
                tape_uuid: [17; 16],
                barcode: None,
                reason: "uncertain write".into(),
                evidence_json: None,
            })
            .unwrap();
        drop(state);
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let code = run_with_standalone_test_io(
            &[
                "rem",
                "--json",
                "--allow-untrusted-state-volume",
                "tape",
                "quarantine",
                "list",
            ],
            root.path(),
            &env.xdg,
            &env.system_config,
            &env.locks,
            vec![],
            |_| panic!("catalog listing must not open a device"),
            &mut out,
            &mut errors,
        );
        assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&out));
        assert!(String::from_utf8_lossy(&out).contains(&fence.quarantine_id));
        assert!(selected_in(&args, &env));
        let without_override: Vec<_> = args
            .iter()
            .filter(|arg| *arg != "--allow-untrusted-state-volume")
            .cloned()
            .collect();
        assert!(selected_in(&without_override, &env));
        for option in [
            "--config=local.toml",
            "--endpoint=server",
            "--library",
            "--after-settled-inventory",
        ] {
            let mut explicit = args.to_vec();
            explicit.push(option.into());
            assert!(!selected_in(&explicit, &env), "{option}");
        }
        fs::write(&env.system_config, "").unwrap();
        assert!(!selected_in(&args, &env));
    }

    #[test]
    fn capacity_failures_preserve_totals_and_name_the_continuation_pool() {
        assert!(super::tape_continuation(
            &remanence_api::MountError::EarlyWarning
        ));
        for cause in [
            remanence_api::PoolWriteError::SelectedTapeInsufficientCapacity {
                object_size: 10,
                raw_capacity: 9,
                used: 0,
            },
            remanence_api::PoolWriteError::TerminalCloseRequired {
                detail: "terminal reserve".into(),
            },
        ] {
            let error = remanence_api::MountError::PoolWrite(Box::new(cause));
            let mut bytes = Vec::new();
            let mut output = super::PutOutput {
                active_fences: Vec::new(),
                out: &mut bytes,
                json: true,
                receipts: Vec::new(),
                summary: None,
                tape_continuation: false,
            };
            output
                .receipt(serde_json::json!({"event":"written","path":"first"}))
                .unwrap();
            output.tape_continuation = super::tape_continuation(&error);
            output
                .finish(Some(&("second".into(), error.to_string())), "archive")
                .unwrap();
            let summary = output.summary.unwrap();
            assert_eq!(summary["written"], 1);
            assert_eq!(summary["failed"]["path"], "second");
            assert_eq!(
                summary["next"],
                "finalize this tape, init the next in pool archive, and re-run the same put"
            );
        }
    }

    /// ENOSPC on the host must retain its path without proposing tape finalization.
    #[test]
    fn host_enospc_has_null_continuation() {
        let error =
            remanence_api::MountError::PoolWrite(Box::new(remanence_api::PoolWriteError::Io {
                context: "write spool",
                path: "/host/spool/object".into(),
                source: std::io::Error::from_raw_os_error(28),
            }));
        let mut bytes = Vec::new();
        let mut output = super::PutOutput {
            active_fences: Vec::new(),
            out: &mut bytes,
            json: true,
            receipts: Vec::new(),
            summary: None,
            tape_continuation: super::tape_continuation(&error),
        };
        output
            .finish(
                Some(&("/host/spool/object".into(), error.to_string())),
                "archive",
            )
            .unwrap();
        let summary = output.summary.unwrap();
        assert!(summary["next"].is_null());
        assert!(summary["failed"]["error"]
            .as_str()
            .unwrap()
            .contains("/host/spool/object"));
    }

    #[test]
    fn supersede_tail_routes_and_parses_exact_confirmation() {
        use clap::Parser;
        let args = [
            "rem",
            "tape",
            "supersede-tail",
            "--device",
            "/dev/sg-test",
            "--acknowledge-tape",
            "HAND",
            "--confirm-supersede",
            "SUPERSEDE HAND",
        ];
        assert!(super::selected(
            &args
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        ));
        let cli = super::Cli::try_parse_from(args).unwrap();
        assert!(
            matches!(cli.command, super::Command::Tape { command: super::Tape::SupersedeTail {
            acknowledge_tape, confirm_supersede: Some(confirmation)
        } } if acknowledge_tape == "HAND" && confirmation == "SUPERSEDE HAND")
        );
    }

    use super::*;
    /// Help is syntax, not a search for the word in option values or operands.
    #[test]
    fn shared_help_preserves_literal_operands_and_both_interfaces() {
        for args in [
            vec!["rem", "put", "help", "--pool", "example"],
            vec!["rem", "put", "--pool", "help", "file"],
            vec!["rem", "get", "help", "--dest", "out"],
            vec!["rem", "tape", "init", "help"],
            vec!["rem", "tape", "finalize", "--reason", "help"],
            vec!["rem", "put", "--pool", "example", "--", "--help"],
        ] {
            let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
            assert!(!selected(&args), "{args:?}");
            assert!(shared_help(&args).is_none(), "{args:?}");
        }
        for args in [
            vec!["rem", "put", "--help"],
            vec!["rem", "get", "-h"],
            vec!["rem", "help", "tape", "init"],
            vec!["rem", "tape", "help", "init"],
            vec!["rem", "tape", "--config", "help", "init", "--help"],
            vec!["rem", "--device", "/dev/test", "tape", "finalize", "--help"],
        ] {
            let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
            let help = shared_help(&args).unwrap();
            assert!(help.contains("Library/daemon interface:"));
            assert!(help.contains("Standalone device interface:"));
        }
    }

    /// Named profiles use the requested media block size; custom geometry stays literal.
    #[test]
    fn named_init_schemes_use_actual_block_size() {
        for size in [262144, 1048576] {
            assert_eq!(
                resolve_init_scheme("default", size).unwrap(),
                remanence_parity::ParityConfig::Scheme(
                    remanence_parity::default_scheme_for_block_size(size)
                )
            );
            assert_eq!(
                resolve_init_scheme("conservative", size).unwrap(),
                remanence_parity::ParityConfig::Scheme(
                    remanence_parity::conservative_scheme_for_block_size(size)
                )
            );
            assert_eq!(
                resolve_init_scheme(HOME_POOL_SCHEME, size).unwrap(),
                remanence_parity::parse_parity_arg(HOME_POOL_SCHEME).unwrap()
            );
        }
        assert_eq!(
            resolve_init_scheme(" default ", 1048576).unwrap(),
            resolve_init_scheme("default", 1048576).unwrap()
        );
        let remanence_parity::ParityConfig::Scheme(scheme) =
            resolve_init_scheme("default", 1048576).unwrap()
        else {
            panic!("parity required")
        };
        assert_eq!(scheme.stripes_per_neighborhood, 128);
    }
    /// Every typed no-authority outcome is a failed verification, including corrupt replicas.
    #[test]
    fn fast_verification_rejects_all_recovery_required_outcomes() {
        use remanence_parity::{
            BotStructuralRecoveryReason, BotStructuralRecoveryRequired, TerminalInventoryOutcome,
            TerminalReplicaEvidence, TerminalReplicaFailure, TerminalReplicaFailureKind,
        };
        for reason in [
            BotStructuralRecoveryReason::NoUsableTerminalLayout,
            BotStructuralRecoveryReason::AllMembersInvalid,
        ] {
            let outcome = TerminalInventoryOutcome::BotStructuralRecoveryRequired(Box::new(
                BotStructuralRecoveryRequired {
                    reason,
                    replicas: std::array::from_fn(|_| {
                        TerminalReplicaEvidence::Invalid(TerminalReplicaFailure {
                            kind: TerminalReplicaFailureKind::Missing,
                            detail: "test".into(),
                        })
                    }),
                },
            ));
            assert!(summarize_terminal(outcome.clone(), true)
                .unwrap_err()
                .contains("verification failed"));
            assert!(
                summarize_terminal(outcome, false).is_ok(),
                "identify can report the recovery requirement"
            );
        }
    }
}
