//! Production standalone dispatch. Dependencies are filesystem roots, enumeration
//! and transport only; the public integration seam runs this same parser and path.
use clap::{Parser, Subcommand};
use remanence_api::{MountBackend, MountTarget, OperatorMount};
use remanence_library::{
    BlockSize, DeviceAttachment, DiscoveryReport, DriveHandle, DriveHandleSink, DriveHandleSource,
    IoErrorKind, SgTransport, StaticAllowlist,
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
    },
    Get {
        object: String,
        #[arg(long)]
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
enum Tape {
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
        #[arg(long)]
        out: PathBuf,
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
    ImportJournals {
        #[arg(long)]
        from: PathBuf,
    },
    Release,
}

/// Select explicit standalone operations before the legacy library discovery path.
pub(crate) fn selected(args: &[OsString]) -> bool {
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
            "doctor" | "drives" | "diagnostics" => true,
            "pool" => words.get(i + 1).is_some_and(|w| w == "new"),
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
}
pub(crate) fn run_live(args: &[OsString]) -> ExitCode {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let env = Environment {
        xdg: std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state")),
        system_config: "/etc/rem/config.toml".into(),
        locks: "/run/lock/rem".into(),
    };
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
    let result = dispatch(&cli, &env, enumeration, factory, &mut observed, errors);
    let value = match &result {
        Ok(v) => json!({"ok":true,"result":v}),
        Err(e) => json!({"ok":false,"status":"failed","error":e}),
    };
    if cli.json {
        let mut value = value;
        value["supported_endpoint_kinds"] = json!(["library", "standalone"]);
        value["observed_endpoints"] = json!(observed);
        let _ = writeln!(out, "{value}");
    } else {
        match &result {
            Ok(v) => {
                let _ = writeln!(out, "{}", serde_json::to_string_pretty(v).unwrap());
            }
            Err(e) => {
                let _ = writeln!(errors, "error: {e}");
            }
        }
    }
    if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
fn read_only(command: &Command) -> bool {
    matches!(
        command,
        Command::Doctor
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
                command: Tape::AssignPool { tape: Some(_), .. }
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
        let mut state =
            StateHandle::open_with_config(StatePaths::from_config(&path, &config), config)
                .map_err(err)?;
        assign(&mut state, *uuid.as_bytes(), pool.as_deref())?;
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
            DiscoveryReport {
                libraries: vec![],
                unattached_drives: vec![],
                complete: warnings.is_empty(),
                incomplete_evidence: warnings.clone(),
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
        if let Some(message) = remanence_library::drive_mapping_message(warning) {
            let _ = writeln!(errors, "{message}");
        }
    }
    match &cli.command {
        Command::Doctor => return Ok(doctor(env, &config, &report)),
        Command::Drives => {
            return Ok(
                json!({"endpoints":observed,"complete":report.complete,"warnings":report.warnings.iter().map(|w|format!("{w:?}")).collect::<Vec<_>>()}),
            )
        }
        Command::Diagnostics {
            command: Diagnostics::Capture { out },
        } => return diagnostics(cli, &report, factory, out),
        _ => {}
    }
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
    let mut state =
        StateHandle::open_with_config(StatePaths::from_config(&path, &config), config.clone())
            .map_err(err)?;
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
    if let Command::Tape {
        command:
            Tape::Init {
                label,
                pool,
                generation,
                block_size,
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
        } => {
            let tape = backend
                .catalog_index()
                .get_tape(&uuid)
                .map_err(err)?
                .ok_or("loaded tape is not catalogued; use tape adopt")?;
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
            let mut files = vec![];
            for p in paths {
                collect_files(
                    p,
                    p.file_name()
                        .map(PathBuf::from)
                        .ok_or("source has no filename")?,
                    &mut files,
                )?;
            }
            if files.is_empty() {
                return Err("put found no regular files to archive".into());
            }
            let mut objects = vec![];
            for (source, archive_path) in files {
                let request = remanence_api::WriteObjectToPoolRequest {
                    pool_id: loaded.clone(),
                    caller_object_id: archive_path.to_string_lossy().into_owned(),
                    archive_path,
                    source: remanence_api::WriteObjectSource::Path(source),
                    expected_content_sha256: None,
                    expected_object_id: None,
                    representation: representation.clone(),
                    input_kind: remanence_api::WriteObjectInputKind::LogicalFile,
                };
                let result = backend
                    .write_checkpointed(&mut mounted, request)
                    .map_err(err)?;
                let mut locator = Vec::new();
                crate::pool_ops::print_locator_json_with_recipients(
                    &result.object,
                    &loaded,
                    None,
                    &mut locator,
                );
                objects.push(serde_json::from_slice::<Value>(&locator).map_err(err)?);
            }
            Ok(json!({"tape":Uuid::from_bytes(uuid),"pool":loaded,"objects":objects}))
        }
        Command::Get {
            object,
            out,
            private_key,
        } => {
            let record = backend
                .catalog_index()
                .get_native_object(object)
                .map_err(err)?
                .or(backend
                    .catalog_index()
                    .get_native_object_by_caller_object_id(object)
                    .map_err(err)?)
                .ok_or("object is not catalogued")?;
            let copy = record
                .copies
                .iter()
                .find(|c| c.tape_uuid == uuid && c.status == "committed")
                .ok_or("object has no committed copy on the loaded tape")?;
            let locator=json!({"object_id":record.object_id,"caller_object_id":record.caller_object_id,"body_format":record.body_format,"pool_id":copy.pool_id,"tape_uuid":crate::bytes_to_hex(&uuid),"tape_file_number":copy.tape_file_number.to_string(),"first_body_lba":copy.first_body_lba.to_string(),"content_sha256":crate::bytes_to_hex(record.content_hash.as_deref().ok_or("object lacks content hash")?)}).to_string();
            let result = crate::pool_ops::restore_mounted_tape_object(
                mounted,
                backend.catalog_index(),
                &locator,
                private_key.as_deref(),
                out,
            );
            backend.finish().map_err(err)?;
            return result;
        }
        Command::Tape { command } => {
            match command {
                Tape::Identify => Ok(
                    json!({"tape_uuid":Uuid::from_bytes(uuid),"label":mounted.label.as_ref().map(|p|&p.0),"generation":mounted.generation.map(|g|g.0.media_code()),"terminal_index":terminal_summary(&mut mounted.drive, false)?}),
                ),
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
                        if !result.is_complete_tape() {
                            Err(format!("full verification incomplete: {result:?}"))
                        } else {
                            Ok(json!({"verified_complete":true}))
                        }
                    } else {
                        terminal_summary(&mut mounted.drive, true)
                    }
                }
                Tape::Restore {
                    out,
                    private_key,
                    scan,
                } => {
                    let key = private_key
                        .as_deref()
                        .map(crate::pool_ops::read_private_key_file)
                        .transpose()?;
                    let report = if *scan {
                        remanence_api::scan_and_restore(&mut mounted.drive, out, key.as_ref())?
                    } else {
                        remanence_api::restore_catalogless(&mut mounted.drive, out, key.as_ref())?
                    };
                    if report["candidates"]
                        .as_array()
                        .is_some_and(|items| items.iter().any(|item| item["status"] != "restored"))
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
                    Ok(json!({"finalized":format!("{result:?}")}))
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
fn collect_files(
    source: &Path,
    archive: PathBuf,
    files: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<(), String> {
    let meta = fs::symlink_metadata(source).map_err(err)?;
    if meta.is_file() {
        files.push((source.into(), archive));
    } else if meta.is_dir() {
        let mut entries = fs::read_dir(source)
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            collect_files(&e.path(), archive.join(e.file_name()), files)?;
        }
    } else {
        return Err(format!("unsupported source type: {}", source.display()));
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
}
fn prepare_init(
    drive: &mut DriveHandle,
    state: &mut StateHandle,
    label: &str,
    pool: &str,
    generation: Option<&str>,
    block_size: Option<u32>,
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
    let facts = remanence_api::project_tape_init_catalog_inputs(
        state.catalog_index(),
        label,
        &bot.classification,
        pool,
    )
    .map_err(err)?;
    let decision = remanence_api::decide_tape_init(
        &bot.classification,
        facts.catalog_row.as_ref(),
        &facts.barcode_state,
        pool,
        bot.physical_data_past_bootstrap,
        &facts.committed_copies,
    );
    if !matches!(
        decision,
        remanence_api::InitDecision::FreshInit | remanence_api::InitDecision::IdempotentNoOp
    ) {
        return Err(format!("init refused: {decision:?}"));
    }
    let uuid = match &bot.classification {
        remanence_api::BotClassification::OursBootstrap { uuid, .. } => *uuid,
        _ => *Uuid::new_v4().as_bytes(),
    };
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
    } = plan;
    let action = remanence_api::maybe_write_tape_init_bootstrap(
        &mut DriveHandleSink(drive),
        &decision,
        remanence_api::TapeInitWriteOptions::default(),
        uuid,
        size,
        parity.clone(),
        &remanence_state::audit::writer_identity(env!("CARGO_PKG_VERSION")),
    )
    .map_err(err)?;
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
                force: false,
            },
            crate::ProvisioningContext {
                provenance: LabelProvenance::Operator,
                observation: Some(observation),
                explicit_pool: Some(&pool),
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
    let result = remanence_parity::read_terminal_index_inventory_streamed(
        &mut remanence_parity::DriveHandleRawSource::new(drive),
        &bootstrap.tape_uuid,
        bootstrap.block_size_bytes,
        |_| Ok(()),
    )
    .map_err(err)?;
    summarize_terminal(result, verifying)
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
                "verification failed: no usable terminal index; recovery required: {:?}",
                recovery.reason
            ))
        }
        _ => Ok(
            json!({"terminal_index":format!("{result:?}"),"status":if verifying { "verified" } else { "identified" }}),
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

#[cfg(test)]
mod tests {
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
