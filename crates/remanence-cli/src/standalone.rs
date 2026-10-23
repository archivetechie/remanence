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
const DEFAULT_FINALIZE_REASON: &str = "finalized with rem";
const NAMESPACE_REMANENCE_FINALIZE: Uuid = Uuid::from_u128(0xc33a391e_7209_5b35_a198_5cab71b91ea4);
const HOME_POOL_SCHEME: &str = "custom:128,4,1";

/// Finalization identity is tape-scoped and deliberately independent of build metadata.
fn default_finalize_key(tape_uuid: [u8; 16]) -> Uuid {
    Uuid::new_v5(&NAMESPACE_REMANENCE_FINALIZE, &tape_uuid)
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Direct standalone operations and host configuration.
#[derive(Debug, Parser)]
#[command(
    name = "rem",
    version,
    about = "library operations go through the daemon; a standalone drive is operated directly."
)]
struct Cli {
    /// Load this configuration instead of home defaults.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Operate this discovered SCSI tape device directly.
    #[arg(long, global = true)]
    device: Option<PathBuf>,
    /// Authorize a drive by its discovered serial or device path.
    #[arg(long, global = true)]
    allow_device: Vec<String>,
    /// Allow reads from a drive that has no VPD serial.
    #[arg(long, global = true)]
    allow_unserialized_device: bool,
    /// Override volume durability checks when intentionally using an untrusted state filesystem.
    #[arg(long, global = true)]
    allow_untrusted_state_volume: bool,
    /// Acknowledge incomplete discovery for read-only access.
    #[arg(long, global = true)]
    acknowledge_unknown_ownership: bool,
    /// Acquire a SCSI reservation when other initiators could access this drive.
    #[arg(long, global = true)]
    reserve: bool,
    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Choose an operation.
    #[command(subcommand)]
    command: Command,
}
/// Available operations for this command.
#[derive(Debug, Subcommand)]
enum Command {
    /// List committed archive paths.
    List {
        /// Filter entries by tape label or UUID.
        #[arg(long, alias = "tape-uuid")]
        tape: Option<String>,
        /// List archive paths beginning with this prefix.
        #[arg(long)]
        path_prefix: Option<String>,
    },
    /// Check this host and save a default standalone drive.
    Setup {
        /// Accept drive selection and any ownership assertion without prompting.
        #[arg(long)]
        yes: bool,
    },
    /// Check permissions and storage readiness.
    Doctor,
    /// Show discovered tape endpoints and ownership.
    Drives,
    /// Capture diagnostic evidence.
    Diagnostics {
        /// Choose an operation.
        #[command(subcommand)]
        command: Diagnostics,
    },
    /// Manage tape pools.
    Pool {
        /// Choose an operation.
        #[command(subcommand)]
        command: Pool,
    },
    /// Operate or inspect a tape.
    Tape {
        /// Choose an operation.
        #[command(subcommand)]
        command: Tape,
    },
    /// Store files on the loaded tape.
    Put {
        /// Files to store on the loaded tape.
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        /// Select the configured tape pool.
        #[arg(long)]
        pool: Option<String>,
        /// Encrypt for this recipient public key.
        #[arg(long)]
        recipient: Vec<PathBuf>,
        /// Permit a plaintext version when the newest committed version is encrypted.
        #[arg(long)]
        allow_plaintext_copy: bool,
        /// Select or verify the tape identity.
        #[arg(long, alias = "tape-uuid")]
        tape: Option<String>,
    },
    /// Restore an object or archive path.
    Get {
        /// Object UUID or archive path to retrieve.
        object: String,
        /// Retrieve this specific object version.
        #[arg(long)]
        version: Option<Uuid>,
        /// Write restored files to this destination.
        #[arg(long, alias = "dest")]
        out: PathBuf,
        /// Decrypt using this private key file.
        #[arg(long)]
        private_key: Option<PathBuf>,
    },
}
/// Available operations for this command.
#[derive(Debug, Subcommand)]
enum Diagnostics {
    /// Capture diagnostic evidence in a tar file.
    Capture {
        /// Write the diagnostic tar archive to this destination.
        #[arg(long)]
        out: PathBuf,
    },
}
/// Available operations for this command.
#[derive(Debug, Subcommand)]
enum Pool {
    #[command(after_help = HOME_PARITY_HELP)]
    /// Create a tape pool.
    New {
        /// Name of the new pool.
        name: String,
        /// Choose the parity geometry.
        #[arg(long)]
        scheme: Option<String>,
        /// Set the fixed tape block size in bytes.
        #[arg(long, value_parser=crate::recovery_report::parse_block_size)]
        block_size: Option<u32>,
    },
}
/// Available operations for this command.
#[derive(Debug, Subcommand)]
enum Quarantine {
    /// List active quarantine fences.
    List,
    /// Release a quarantine fence after recording an acknowledgement.
    Release {
        /// Identifier of the quarantine fence to release.
        id: String,
        /// Record the reason for releasing this fence.
        #[arg(long)]
        ack: String,
    },
}
/// Available operations for this command.
#[derive(Debug, Subcommand)]
enum Tape {
    /// Inspect and release local tape fences.
    Quarantine {
        /// Choose an operation.
        #[command(subcommand)]
        command: Quarantine,
    },
    /// Read the loaded tape identity.
    Identify,
    #[command(after_help = HOME_PARITY_HELP)]
    /// Initialize a cartridge with an operator label.
    Init {
        /// Operator label printed on the cartridge.
        #[arg(long)]
        label: String,
        /// Select the configured tape pool.
        #[arg(long)]
        pool: Option<String>,
        /// Specify the cartridge generation, such as L8.
        #[arg(long)]
        generation: Option<String>,
        /// Set the fixed tape block size in bytes.
        #[arg(long, value_parser=crate::recovery_report::parse_block_size)]
        block_size: Option<u32>,
        /// Erase foreign or existing data after exact label confirmation.
        #[arg(long)]
        clobber_data: bool,
        /// Non-interactive confirmation: CLOBBER followed by the exact label.
        #[arg(long, requires = "clobber_data")]
        confirm_clobber: Option<String>,
    },
    /// Import the loaded tape identity into the catalog.
    Adopt {
        /// Operator label printed on the cartridge.
        #[arg(long)]
        label: Option<String>,
        /// Select the configured tape pool.
        #[arg(long)]
        pool: Option<String>,
    },
    /// Verify the loaded tape.
    Verify {
        /// Verify all object data.
        #[arg(long)]
        full: bool,
    },
    /// Recover archive members from the loaded tape.
    Restore {
        /// Write restored files to this destination.
        #[arg(long, alias = "dest", required_unless_present = "list")]
        out: Option<PathBuf>,
        /// List recoverable archive members without extracting.
        #[arg(long)]
        list: bool,
        /// Restore only this archive member path.
        #[arg(long)]
        path: Vec<String>,
        /// Decrypt using this private key file.
        #[arg(long)]
        private_key: Option<PathBuf>,
        /// Scan tape objects instead of using the terminal index.
        #[arg(long)]
        scan: bool,
    },
    /// Close the tape and write its terminal index.
    Finalize {
        /// Select or verify the tape identity.
        #[arg(long, alias = "tape-uuid")]
        tape: Option<Uuid>,
        /// Confirm the loaded tape by label or UUID.
        #[arg(long, alias = "ack-tape-uuid")]
        acknowledge_tape: String,
        /// Record a finalization reason (default: finalized with rem).
        #[arg(long)]
        reason: Option<String>,
        /// Override the stable per-tape finalization request key.
        #[arg(long)]
        idempotency_key: Option<Uuid>,
        /// Require this pool instead of the catalogued pool.
        #[arg(long)]
        expected_pool: Option<String>,
    },
    /// Change the catalogued pool assignment.
    AssignPool {
        /// Select or verify the tape identity.
        #[arg(long, alias = "tape-uuid")]
        tape: Option<Uuid>,
        #[arg(
            long,
            conflicts_with = "unassign",
            required_unless_present = "unassign"
        )]
        /// Select the configured tape pool.
        pool: Option<String>,
        /// Remove the tape from its pool.
        #[arg(long)]
        unassign: bool,
    },
    /// Bind a cartridge label to a new identity.
    Rebind {
        /// Operator label printed on the cartridge.
        #[arg(long)]
        label: String,
        /// Require this previous tape UUID before rebinding.
        #[arg(long)]
        previous_uuid: Uuid,
    },
    /// Record the cartridge generation.
    SetGeneration {
        /// Specify the cartridge generation, such as L8.
        #[arg(long)]
        generation: String,
    },
    /// Explicitly discard a suffix beyond local checkpoint authority.
    SupersedeTail {
        /// Confirm the loaded tape by label or UUID.
        #[arg(long, alias = "ack-tape-uuid")]
        acknowledge_tape: String,
        /// Confirm discarding the reported suffix with SUPERSEDE followed by its label.
        #[arg(long)]
        confirm_supersede: Option<String>,
    },
    /// Import checkpoint journals for append recovery.
    ImportJournals {
        /// Import checkpoint journals from this directory.
        #[arg(long)]
        from: PathBuf,
    },
    /// Clear the drive's removal guard; quarantine fences remain active.
    Release,
}

/// Select explicit standalone operations before the legacy library discovery path.
pub(crate) fn selected(args: &[OsString]) -> bool {
    selected_in(args, &Environment::from_process())
}

fn selected_in(args: &[OsString], env: &Environment) -> bool {
    use clap::CommandFactory;
    fn options(command: &clap::Command, values: &mut std::collections::HashSet<String>) {
        for arg in command.get_arguments() {
            if arg.get_action().takes_values() {
                if let Some(long) = arg.get_long() {
                    values.insert(format!("--{long}"));
                }
                if let Some(aliases) = arg.get_all_aliases() {
                    for alias in aliases {
                        values.insert(format!("--{alias}"));
                    }
                }
            }
        }
        for child in command.get_subcommands() {
            options(child, values);
        }
    }
    let mut values = std::collections::HashSet::new();
    options(&Cli::command(), &mut values);
    options(&crate::Cli::command(), &mut values);
    let mut words = Vec::new();
    let mut explicit_device = false;
    let mut library = false;
    let mut args = args.iter().skip(1);
    while let Some(arg) = args.next() {
        let word = arg.to_string_lossy();
        if word == "--" {
            break;
        }
        if word.starts_with('-') {
            let flag = word.split('=').next().unwrap();
            explicit_device |= flag == "--device";
            library |= matches!(
                flag,
                "--config" | "--endpoint" | "--library" | "--after-settled-inventory"
            );
            if !word.contains('=') && values.contains(flag) {
                args.next();
            }
        } else {
            words.push(word.into_owned());
        }
    }
    if explicit_device {
        return true;
    }
    if words.first().is_some_and(|w| w == "help") {
        words.remove(0);
    }
    if words
        .first()
        .is_some_and(|w| matches!(w.as_str(), "tape" | "pool"))
        && words.get(1).is_some_and(|w| w == "help")
    {
        words.remove(1);
    }
    let home = !library
        && env.system_config.try_exists().is_ok_and(|exists| !exists)
        && load(&env.xdg.join("rem/config.toml"), true)
            .ok()
            .and_then(|c| c.home)
            .and_then(|h| h.drive_serial)
            .is_some();
    match words.first().map(String::as_str) {
        Some("setup" | "doctor" | "drives" | "diagnostics" | "list") => true,
        Some("pool") => words.get(1).is_some_and(|w| w == "new"),
        Some("put" | "get") => home,
        Some("tape") => match words.get(1).map(String::as_str) {
            Some(
                "identify" | "verify" | "adopt" | "restore" | "assign-pool" | "rebind"
                | "set-generation" | "import-journals" | "release",
            ) => true,
            Some("init" | "finalize" | "supersede-tail" | "quarantine") | None => home,
            _ => false,
        },
        _ => false,
    }
}
/// Show the combined command index; command-specific help follows normal routing.
pub(crate) fn shared_help(args: &[OsString]) -> Option<String> {
    use clap::CommandFactory;
    if args.len() != 2 || !matches!(args[1].to_str(), Some("--help" | "-h" | "help")) {
        return None;
    }
    let mut root = crate::Cli::command();
    let standalone = Cli::command();
    for name in ["setup", "drives", "doctor", "list"] {
        if root.find_subcommand(name).is_none() {
            root = root.subcommand(standalone.find_subcommand(name).unwrap().clone());
        }
    }
    Some(root.render_long_help().to_string())
}

struct Environment {
    xdg: PathBuf,
    system_config: PathBuf,
    locks: PathBuf,
    test_input: bool,
    euid: u32,
    doctor_checks: Option<Value>,
}
/// Resolve the state root consistently for process and fixture inputs.
fn state_home(home: &Path, xdg: Option<OsString>) -> PathBuf {
    xdg.filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"))
}
impl Environment {
    fn from_process() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Environment {
            xdg: state_home(&home, std::env::var_os("XDG_STATE_HOME")),
            system_config: if std::env::var_os("REM_TEST_NO_LIVE_SCSI").is_some() {
                std::env::var_os("REM_TEST_SYSTEM_CONFIG").map(PathBuf::from)
            } else {
                None
            }
            .unwrap_or_else(|| "/etc/rem/config.toml".into()),
            locks: "/run/lock/rem".into(),
            test_input: false,
            euid: unsafe { libc::geteuid() },
            doctor_checks: None,
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
        euid: 1000,
        doctor_checks: None,
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
        euid: 1000,
        doctor_checks: None,
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
    let doctor_not_ready =
        matches!(cli.command, Command::Doctor) && result.as_ref().is_ok_and(|v| v["ready"] != true);
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
        Ok(v) if doctor_not_ready => {
            json!({"ok":false,"error":format!("not ready: {}", v["checks"].as_array().unwrap().iter().filter(|c| c["ok"] != true).map(|c| c["name"].as_str().unwrap_or("unknown")).collect::<Vec<_>>().join(", ")),"result":v})
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
            Ok(v) if matches!(cli.command, Command::Setup { .. }) => {
                let _ = writeln!(
                    out,
                    "saved {} serial {} as this host's drive",
                    cell(&v["vendor_product"]),
                    cell(&v["drive_serial"])
                );
            }
            Ok(v) if matches!(cli.command, Command::Doctor) => {
                let _ = render_doctor(v, out);
            }
            Ok(_) if matches!(cli.command, Command::Drives) => {
                let _ = render_drives(&observed, out);
            }
            Ok(v) if matches!(cli.command, Command::List { .. }) => {
                let mut rows = vec![vec![
                    "POOL",
                    "PATH",
                    "SIZE",
                    "VERSIONS",
                    "NEWEST OBJECT",
                    "TAPES",
                    "FINALIZED",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()];
                for row in v["rows"].as_array().into_iter().flatten() {
                    rows.push(vec![
                        cell(&row["pool"]),
                        cell(&row["path"]),
                        cell(&row["size"]),
                        cell(&row["version_count"]),
                        cell(&row["newest_object_id"]),
                        row["tapes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(cell)
                            .collect::<Vec<_>>()
                            .join(", "),
                        cell(&row["finalized"]),
                    ]);
                }
                let _ = render_table(&rows, out);
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
    if result.is_ok() && !incomplete_finalization && !doctor_not_ready {
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

/// Build first-use configuration without persisting operator choices.
fn default_home_config(root: &Path) -> String {
    let q = |p: PathBuf| toml::Value::String(p.to_string_lossy().into_owned()).to_string();
    format!("[daemon]\nstate_dir = {}\ndefault_idle_timeout_seconds = 1800\n[journal]\ndir = {}\n[audit]\ndir = {}\nfsync = true\n[index]\nsqlite_path = {}\n[cache]\ntape_catalog_dir = {}\n[[tape_pools]]\nid = \"default\"\nscheme = \"custom:128,4,1\"\n",q(root.to_path_buf()),q(root.join("journals")),q(root.join("audit")),q(root.join("index/rem-state.sqlite")),q(root.join("cache/tapes")))
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
        let text = default_home_config(&root);
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
    load_for_use(path, override_trust, false)
}

/// Validate through the existing loader while preserving policy for configuration edits.
fn load_for_use(
    path: &Path,
    override_trust: bool,
    preserve_policy: bool,
) -> Result<remanence_state::RemConfig, String> {
    let mut doc: toml::Value =
        toml::from_str(&fs::read_to_string(path).map_err(err)?).map_err(err)?;
    let require_trusted_volume = doc
        .get("journal")
        .and_then(|j| j.get("require_trusted_volume"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    if override_trust {
        doc.get_mut("journal")
            .and_then(toml::Value::as_table_mut)
            .ok_or("missing journal config")?
            .insert("require_trusted_volume".into(), toml::Value::Boolean(false));
    }
    let mut config = remanence_state::parse_config_toml(&toml::to_string(&doc).map_err(err)?)
        .map_err(|e| {
            format!(
            "{e}; use --allow-untrusted-state-volume only to explicitly override the volume check"
        )
        })?;
    if preserve_policy {
        config.journal.require_trusted_volume = require_trusted_volume;
    }
    Ok(config)
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
/// Root must explicitly select configuration before any home-state side effects.
fn refuse_root(explicit_config: bool, system_config: bool, root: bool) -> bool {
    root && !explicit_config && !system_config
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
    if refuse_root(
        cli.config.is_some(),
        env.system_config.exists(),
        env.euid == 0,
    ) {
        return Err(
            "rem is not meant to run as root; fix permissions with `rem doctor` (or pass --config)"
                .into(),
        );
    }
    if matches!(cli.command, Command::Setup { .. })
        && (cli.config.is_some() || env.system_config.exists())
    {
        return Err(
            "setup saves a home default; library hosts select drives through their daemon config"
                .into(),
        );
    }
    if cli.device.is_some()
        && cli.allow_device.is_empty()
        && !matches!(cli.command, Command::Setup { .. })
    {
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
    let setup = matches!(cli.command, Command::Setup { .. });
    let discovery_only = matches!(
        cli.command,
        Command::Setup { .. } | Command::Doctor | Command::Drives | Command::Diagnostics { .. }
    );
    let needs_device = !discovery_only
        && !matches!(
            cli.command,
            Command::Pool { .. }
                | Command::Tape {
                    command: Tape::Quarantine { .. } | Tape::AssignPool { tape: Some(_), .. }
                }
        );
    if needs_device && cli.device.is_none() {
        let home = env.xdg.join("rem/config.toml");
        let saved = cli.config.is_none()
            && !env.system_config.exists()
            && home.exists()
            && load(&home, true)?
                .home
                .and_then(|h| h.drive_serial)
                .is_some();
        if !saved {
            return Err("this command requires --device PATH and --allow-device <serial|PATH>; run rem setup to save a default".into());
        }
    }
    let path = if discovery_only && cli.config.is_none() && !env.system_config.exists() {
        if !env.xdg.is_absolute() {
            return Err("home state path must be absolute".into());
        }
        env.xdg.join("rem/config.toml")
    } else {
        config_path(cli, env)?
    };
    let config = if discovery_only && !path.exists() {
        toml::from_str(&default_home_config(path.parent().unwrap())).map_err(err)?
    } else {
        load_for_use(
            &path,
            cli.allow_untrusted_state_volume || discovery_only,
            setup,
        )?
    };
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
        Command::Setup { .. } | Command::Doctor | Command::Drives | Command::Diagnostics { .. }
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
    let mut health = matches!(cli.command, Command::Setup { .. } | Command::Doctor)
        .then(|| doctor(env, &config));
    let enumeration = enumeration().map_err(err)?;
    let mut report = match remanence_library::discover_enumerated_with(enumeration, &mut *factory) {
        Ok(r) => r,
        Err(remanence_library::DiscoveryError::NoLibraries { warnings }) => {
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
    for endpoint in observed.iter_mut() {
        if let Some(drive) = report
            .unattached_drives
            .iter()
            .find(|d| endpoint["path"].as_str() == d.sg_path.to_str())
        {
            endpoint["vendor_product"] = json!(format!(
                "{} {}",
                String::from_utf8_lossy(&drive.inquiry.vendor).trim(),
                String::from_utf8_lossy(&drive.inquiry.product).trim()
            ));
        }
    }
    for library in &report.libraries {
        for endpoint in observed.iter_mut() {
            if endpoint["path"].as_str() == library.changer_sg.to_str() {
                endpoint["ownership"] = json!("library");
                endpoint["vendor_product"] = json!(format!(
                    "{} {}",
                    String::from_utf8_lossy(&library.changer_inquiry.vendor).trim(),
                    String::from_utf8_lossy(&library.changer_inquiry.product).trim()
                ));
            }
            for drive in library
                .drive_bays
                .iter()
                .filter_map(|b| b.installed.as_ref())
            {
                if drive
                    .sg_path
                    .as_ref()
                    .and_then(|p| p.to_str())
                    .is_some_and(|p| endpoint["path"].as_str() == Some(p))
                {
                    endpoint["vendor_product"] = json!(format!(
                        "{} {}",
                        drive.vendor.as_deref().unwrap_or("-"),
                        drive.product.as_deref().unwrap_or("-")
                    ));
                }
            }
        }
    }
    for warning in &report.warnings {
        if matches!(
            warning,
            remanence_library::DiscoveryWarning::UnclaimedTape { .. }
        ) && !matches!(cli.command, Command::Drives | Command::Doctor)
        {
            continue;
        }
        let _ = writeln!(errors, "{}", discovery_message(warning));
    }
    if let Some(health) = &mut health {
        finish_doctor_discovery(health, &report);
    }
    match &cli.command {
        Command::Setup { yes } => {
            let checks = health.take().expect("setup checks");
            let setup_out: &mut dyn Write = if cli.json { errors } else { output.out };
            if !yes {
                render_doctor(&checks, setup_out)?;
            }
            let candidates: Vec<_> = report
                .unattached_drives
                .iter()
                .filter(|d| cli.device.as_ref().is_none_or(|p| *p == d.sg_path))
                .collect();
            if candidates.is_empty() {
                return Err("no standalone tape drive found; check cabling and permissions, then run rem drives".into());
            }
            if candidates.len() != 1 {
                render_drives(observed, setup_out)?;
                return Err("several standalone drives found; disconnect drives until exactly one remains or pass rem setup --device PATH".into());
            }
            let drive = candidates[0];
            let vendor_product = format!(
                "{} {}",
                String::from_utf8_lossy(&drive.inquiry.vendor).trim(),
                String::from_utf8_lossy(&drive.inquiry.product).trim()
            );
            let serial = drive
                .serial
                .clone()
                .filter(|s| !s.is_empty())
                .ok_or("cannot save a drive without a VPD serial")?;
            if !confirm_setup(
                &format!(
                    "save {vendor_product} serial {serial} at {}? [y/N]",
                    drive.sg_path.display()
                ),
                *yes,
                env,
                setup_out,
            )? {
                return Err("setup cancelled; no drive saved".into());
            }
            let pin = !report.complete;
            if pin && !confirm_setup("Discovery is incomplete; a standalone pin asserts this serial belongs to no library on this host. Save this ownership assertion? [y/N]", *yes, env, setup_out)? {
                return Err("setup cancelled; no drive saved".into());
            }
            let generated = default_home_config(path.parent().ok_or("config has no parent")?);
            let original = if path.exists() {
                fs::read_to_string(&path).map_err(err)?
            } else {
                generated.clone()
            };
            // Edit one key while preserving unrelated settings, comments and formatting.
            let instruction = format!(
                "set drive_serial = {} under [home] in {}",
                toml::Value::String(serial.clone()),
                path.display()
            );
            let mut document = original
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| format!("cannot edit config: {e}; {instruction}"))?;
            if !document.contains_key("home") {
                document["home"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            let home = document["home"]
                .as_table_like_mut()
                .ok_or_else(|| format!("home is not a table; {instruction}"))?;
            let mut value = toml_edit::Value::from(serial.clone());
            if let Some(old) = home.get("drive_serial").and_then(toml_edit::Item::as_value) {
                *value.decor_mut() = old.decor().clone();
            }
            home.insert("drive_serial", toml_edit::Item::Value(value));
            let mut saved = document.to_string();
            if pin && !config.standalone_drives.iter().any(|d| d.serial == serial) {
                saved.push_str(&format!(
                    "\n[[standalone_drives]]\nserial = {}\n",
                    toml::Value::String(serial.clone())
                ));
            }
            // Validate using the ordinary loader's trust override without changing the saved policy.
            let mut validation: toml::Value = toml::from_str(&saved).map_err(err)?;
            validation
                .get_mut("journal")
                .and_then(toml::Value::as_table_mut)
                .ok_or("missing journal config")?
                .insert("require_trusted_volume".into(), toml::Value::Boolean(false));
            remanence_state::parse_config_toml(&toml::to_string(&validation).map_err(err)?)
                .map_err(err)?;
            let parent = path.parent().ok_or("config has no parent")?;
            fs::create_dir_all(parent).map_err(err)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(err)?;
            temp.write_all(saved.as_bytes()).map_err(err)?;
            temp.as_file().sync_all().map_err(err)?;
            temp.persist(&path).map_err(err)?;
            fs::File::open(parent)
                .and_then(|f| f.sync_all())
                .map_err(err)?;
            return Ok(
                json!({"drive_serial":serial,"vendor_product":vendor_product,"config":path,"ready":checks["ready"]}),
            );
        }
        Command::Doctor => {
            let mut result = health.take().expect("doctor checks");
            if let Some(error) = &catalog_error {
                result["ready"] = json!(false);
                result["checks"].as_array_mut().unwrap().push(json!({"name":"catalog", "ok":false,"detail":error,"fix":"repair or restore the catalog before using this drive"}));
            }
            result["catalog_fence_check"] =
                json!({"ok": catalog_error.is_none(), "error": catalog_error});
            return Ok(result);
        }
        Command::Drives => {
            return Ok(
                json!({"endpoints":observed,"complete":report.complete,"warnings":report.warnings.iter().map(discovery_message).collect::<Vec<_>>()}),
            )
        }
        Command::Diagnostics {
            command: Diagnostics::Capture { out },
        } => return diagnostics(cli, &report, factory, out),
        _ => {}
    }
    let mut state = state.expect("device state");
    let saved_serial = config
        .home
        .as_ref()
        .and_then(|home| home.drive_serial.as_ref());
    let default_device = if cli.device.is_none() {
        let serial = saved_serial.ok_or("this command requires --device PATH and --allow-device <serial|PATH>; run rem setup to save a default")?;
        let matches: Vec<_> = report
            .unattached_drives
            .iter()
            .filter(|d| d.serial.as_ref() == Some(serial))
            .collect();
        if matches.len() != 1 {
            return Err(format!("saved drive {serial} must match exactly one standalone endpoint; tape endpoints found: {}", endpoint_paths(observed)));
        }
        Some(matches[0].sg_path.clone())
    } else {
        None
    };
    let device = cli.device.as_ref().or(default_device.as_ref()).unwrap();
    let drive = report
        .unattached_drives
        .iter()
        .find(|d| d.sg_path == *device)
        .ok_or_else(|| {
            for library in &report.libraries {
                for bay in &library.drive_bays {
                    if bay.installed.as_ref().and_then(|d| d.sg_path.as_ref()) == Some(device) {
                        return format!("device is not an unattached drive; library operations go through the daemon; library {} bay 0x{:04x}", library.serial, bay.element_address);
                    }
                }
            }
            format!("no SCSI device at {}; tape endpoints found: {}", device.display(), endpoint_paths(observed))
        })?;
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
    let authorization =
        StaticAllowlist::new(if cli.device.is_none() && cli.allow_device.is_empty() {
            vec![saved_serial.unwrap().clone()]
        } else {
            cli.allow_device.clone()
        });
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
        } => tape.map(|t| *t.as_bytes()),
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
                    let row = backend
                        .catalog_index()
                        .get_tape(&uuid)
                        .map_err(err)?
                        .ok_or("loaded tape is not catalogued")?;
                    if Uuid::parse_str(acknowledge_tape)
                        .ok()
                        .map(|u| *u.as_bytes())
                        != Some(uuid)
                        && row.voltag.as_deref() != Some(acknowledge_tape.as_str())
                    {
                        return Err("--acknowledge-tape does not match the loaded tape".into());
                    }
                    let result = backend
                        .finalize(
                            &mut mounted,
                            remanence_api::StandaloneFinalizeRequest {
                                tape_uuid: *tape.unwrap_or(Uuid::from_bytes(uuid)).as_bytes(),
                                acknowledge_tape: uuid,
                                reason: reason
                                    .clone()
                                    .unwrap_or_else(|| DEFAULT_FINALIZE_REASON.into()),
                                idempotency_key: idempotency_key
                                    .unwrap_or_else(|| default_finalize_key(uuid)),
                                expected_pool_id: expected_pool.clone().or(row.pool_id),
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
                        "; failed{}: {e}{}",
                        if p.as_os_str().is_empty() { String::new() } else { format!(" {}", p.display()) },
                        next.as_ref().map(|n| format!("; {n}")).unwrap_or_default()
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
        return Err(format!("{}", ready.readiness));
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
fn doctor(env: &Environment, config: &remanence_state::RemConfig) -> Value {
    if let Some(checks) = &env.doctor_checks {
        return checks.clone();
    }
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
    let state_writable = fs::create_dir_all(&config.daemon.state_dir)
        .and_then(|_| tempfile::NamedTempFile::new_in(&config.daemon.state_dir))
        .map(|_| ())
        .map_err(err);
    let mut trusted = config.clone();
    trusted.journal.require_trusted_volume = true;
    let trust = remanence_state::validate_trusted_volume_paths(&trusted).map_err(err);
    let check = |name: &str, ok: bool, detail: String, fix: &str| {
        let mut value = json!({"name":name,"ok":ok,"detail":detail});
        if !ok {
            value["fix"] = json!(fix);
        }
        value
    };
    let checks = vec![
        check(
            "tape group",
            tape_group,
            "effective group membership".into(),
            "add this user to the tape group and log in again",
        ),
        check(
            "capability",
            cap_sys_rawio,
            binary
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "cannot locate this executable".into()),
            "setcap cap_sys_rawio+ep on the executable shown here",
        ),
        check(
            "lock directory",
            lock_writable.is_ok(),
            format!(
                "{}{}",
                env.locks.display(),
                lock_writable
                    .as_ref()
                    .err()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            ),
            "create /run/lock/rem with permissions for the tape group",
        ),
        check(
            "state directory",
            state_writable.is_ok(),
            format!(
                "{}{}",
                config.daemon.state_dir.display(),
                state_writable
                    .as_ref()
                    .err()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            ),
            "make the state directory writable by this user",
        ),
        check(
            "trusted volume",
            trust.is_ok(),
            trust
                .err()
                .unwrap_or_else(|| "state paths support trusted durability".into()),
            "move state to a trusted local filesystem",
        ),
    ];
    let fixes: serde_json::Map<String, Value> = checks
        .iter()
        .filter_map(|check| {
            check.get("fix").map(|fix| {
                let name = match check["name"].as_str().unwrap() {
                    "tape group" => "tape_group",
                    "lock directory" => "lock_dir",
                    "state directory" => "state_dir",
                    "trusted volume" => "trusted_volume",
                    other => other,
                };
                (name.to_owned(), fix.clone())
            })
        })
        .collect();
    json!({"ready":checks.iter().all(|c| c["ok"] == true),"checks":checks,"fixes":fixes,"tape_group":tape_group,"cap_sys_rawio_on_this_binary":cap_sys_rawio,"binary":binary,"state_dir":config.daemon.state_dir,"lock_dir":env.locks,"lock_dir_writable":lock_writable.is_ok(),"lock_error":lock_writable.err(),"drives":[]})
}
/// Add discovery readiness after host permission and state checks have run.
fn finish_doctor_discovery(health: &mut Value, report: &DiscoveryReport) {
    let standalone = report
        .unattached_drives
        .iter()
        .filter(|d| d.ownership == remanence_library::DriveOwnership::Standalone)
        .count();
    let unknown = report.unattached_drives.len() - standalone;
    let present = standalone > 0;
    let mut check = json!({"name":"standalone drive", "ok":present, "detail":format!("{standalone} standalone drives found; {unknown} drives with unknown ownership")});
    if !present {
        check["fix"] = json!("connect a tape drive, check permissions with rem drives, and resolve unknown ownership with rem setup");
    }
    if let Some(fix) = check.get("fix") {
        health["fixes"]["standalone_drive"] = fix.clone();
    }
    health["checks"].as_array_mut().unwrap().push(check);
    health["ready"] = json!(health["checks"]
        .as_array()
        .unwrap()
        .iter()
        .all(|c| c["ok"] == true));
    health["drives"] = json!(remanence_api::observed_endpoint_json(report));
}

/// Share readable discovery warnings between JSON and human output.
fn discovery_message(warning: &remanence_library::DiscoveryWarning) -> String {
    use remanence_library::DiscoveryWarning;
    match warning {
        DiscoveryWarning::UnclaimedTape { sg_path, serial } => format!(
            "tape drive {} (serial {serial}) is not claimed by a library",
            sg_path.display()
        ),
        DiscoveryWarning::DeviceUnreachable { path, source } => {
            format!("cannot inspect {}: {source}", path.display())
        }
        DiscoveryWarning::ScsiError {
            path,
            command,
            summary,
        } => format!("{}: {command} failed: {summary}", path.display()),
        DiscoveryWarning::DriveSerialAmbiguous {
            sg_path,
            serial,
            claimants,
        } => format!(
            "drive {} serial {serial} is claimed by multiple bays: {}",
            sg_path.display(),
            claimants.join(", ")
        ),
        DiscoveryWarning::UnresolvedDrive {
            library,
            serial,
            element_address,
        } => format!(
            "library {library} bay 0x{element_address:04x}: drive serial {serial} is unreachable"
        ),
        DiscoveryWarning::LayoutMismatch { library } => {
            format!("library {library}: reported layouts disagree; using element status")
        }
        DiscoveryWarning::MalformedVoltag {
            library,
            element_address,
        } => {
            format!("library {library} element 0x{element_address:04x}: unreadable cartridge label")
        }
        DiscoveryWarning::DriveMappingDerived { library, method } => format!(
            "library {library}: drive mapping derived by {method}; operations need explicit opt-in"
        ),
        DiscoveryWarning::DriveMappingUnavailable { .. } => {
            remanence_library::drive_mapping_message(warning).unwrap_or_default()
        }
        DiscoveryWarning::NotTapeOrChanger { .. } => {
            warning.informational_message().unwrap_or_default()
        }
    }
}

/// Render aligned diagnostics without hiding actionable failures in JSON.
fn render_doctor(value: &Value, out: &mut dyn Write) -> Result<(), String> {
    let rows: Vec<Vec<String>> = value["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|check| {
            vec![
                if check["ok"] == true {
                    "✓".into()
                } else {
                    "✗".into()
                },
                cell(&check["name"]),
                format!(
                    "{}{}",
                    cell(&check["detail"]),
                    check["fix"]
                        .as_str()
                        .map(|fix| format!("; fix: {fix}"))
                        .unwrap_or_default()
                ),
            ]
        })
        .collect();
    render_table(&rows, out)?;
    writeln!(
        out,
        "{}",
        if value["ready"] == true {
            "ready to use the drive"
        } else {
            "fix the items above"
        }
    )
    .map_err(err)
}
/// Summarize discovery without leaking a JSON object into human errors.
fn endpoint_paths(endpoints: &[Value]) -> String {
    if endpoints.is_empty() {
        "none".into()
    } else {
        endpoints
            .iter()
            .map(|v| v["path"].as_str().unwrap_or("unresolved"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}
/// Render the discovered endpoint inventory as a table.
fn render_drives(endpoints: &[Value], out: &mut dyn Write) -> Result<(), String> {
    let mut rows = vec![
        vec!["PATH", "KIND", "OWNERSHIP", "VENDOR PRODUCT", "SERIAL"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    ];
    let mut endpoints: Vec<_> = endpoints.iter().collect();
    endpoints.sort_by_key(|e| {
        let p = cell(&e["path"]);
        let n = p
            .strip_prefix("/dev/sg")
            .and_then(|n| n.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        (n, p)
    });
    for endpoint in endpoints {
        rows.push(vec![
            cell(&endpoint["path"]),
            cell(&endpoint["kind"]),
            cell(&endpoint["ownership"]),
            cell(&endpoint["vendor_product"]),
            cell(endpoint.get("drive_serial").unwrap_or(&endpoint["serial"])),
        ]);
    }
    render_table(&rows, out)
}
/// Format a scalar cell, including absent discovery facts.
fn cell(value: &Value) -> String {
    if value.is_null() {
        "-".into()
    } else {
        value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| value.to_string())
    }
}
/// Align human tables by the widest cell in each column.
fn render_table(rows: &[Vec<String>], out: &mut dyn Write) -> Result<(), String> {
    let count = rows.first().map(Vec::len).unwrap_or(0);
    let widths: Vec<_> = (0..count)
        .map(|i| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in rows {
        for (i, value) in row.iter().enumerate() {
            write!(out, "{value}").map_err(err)?;
            if i + 1 < count {
                write!(out, "{}", " ".repeat(widths[i] - value.chars().count() + 2))
                    .map_err(err)?;
            }
        }
        writeln!(out).map_err(err)?;
    }
    Ok(())
}
/// Obtain a separate affirmative decision for drive selection and ownership.
fn confirm_setup(
    prompt: &str,
    yes: bool,
    env: &Environment,
    out: &mut dyn Write,
) -> Result<bool, String> {
    if yes {
        return Ok(true);
    }
    writeln!(out, "{prompt}").map_err(err)?;
    out.flush().map_err(err)?;
    if env.test_input {
        return Ok(false);
    }
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).map_err(err)?;
    Ok(matches!(answer.trim(), "y" | "Y"))
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
    /// Derived mappings remain actionable warnings instead of panicking.
    #[test]
    fn derived_mapping_warning_is_readable() {
        let warning = remanence_library::DiscoveryWarning::DriveMappingDerived {
            library: "LIB".into(),
            method: "topology",
        };
        assert_eq!(
            super::discovery_message(&warning),
            "library LIB: drive mapping derived by topology; operations need explicit opt-in"
        );
    }

    /// Every user-facing option must explain itself in generated help.
    #[test]
    fn every_standalone_flag_has_help() {
        use clap::CommandFactory;
        fn check(command: &clap::Command) {
            for arg in command.get_arguments() {
                assert!(
                    arg.get_help()
                        .is_some_and(|help| !help.to_string().trim().is_empty()),
                    "{}: {} has no help",
                    command.get_name(),
                    arg.get_id()
                );
            }
            for child in command.get_subcommands() {
                check(child);
            }
        }
        check(&super::Cli::command());
    }

    /// Selection follows syntax, not strings embedded in option values or operands.
    #[test]
    fn routing_matrix_and_option_operands() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        let env = Environment {
            xdg: root.path().into(),
            system_config: root.path().join("etc.toml"),
            locks: root.path().join("locks"),
            test_input: true,
            euid: 1000,
            doctor_checks: None,
        };
        let path = config_path(&Cli::try_parse_from(["rem", "doctor"]).unwrap(), &env).unwrap();
        let original = fs::read_to_string(&path).unwrap();
        for saved in [false, true] {
            fs::write(
                &path,
                if saved {
                    format!("{original}\n[home]\ndrive_serial = \"DRIVE\"\n")
                } else {
                    original.clone()
                },
            )
            .unwrap();
            for system in [false, true] {
                if system {
                    fs::write(&env.system_config, "").unwrap();
                } else if env.system_config.exists() {
                    fs::remove_file(&env.system_config).unwrap();
                }
                for (words, expected) in [
                    (vec!["tape", "retire", "LABEL"], false),
                    (vec!["tape", "quarantine", "list"], saved && !system),
                    (
                        vec!["tape", "supersede-tail", "--acknowledge-tape", "LABEL"],
                        saved && !system,
                    ),
                    (vec!["put", "file"], saved && !system),
                    (vec!["get", "object", "--out", "out"], saved && !system),
                    (vec!["tape", "init", "BARCODE"], saved && !system),
                    (vec!["tape", "init", "--label", "LABEL"], saved && !system),
                    (
                        vec!["tape", "--config", "daemon.toml", "init", "BARCODE"],
                        false,
                    ),
                    (
                        vec!["tape", "init", "--config=daemon.toml", "BARCODE"],
                        false,
                    ),
                    (vec!["put", "file", "--endpoint", "unix:/daemon"], false),
                    (vec!["put", "--config", "--device", "file"], false),
                    (vec!["put", "--", "--device"], saved && !system),
                    (
                        vec!["put", "--config", "daemon.toml", "--", "--device"],
                        false,
                    ),
                    (vec!["put", "--device", "/dev/test"], true),
                    (vec!["doctor"], true),
                    (vec!["drives"], true),
                    (vec!["setup"], true),
                    (vec!["list"], true),
                    (vec!["pool", "new", "backup"], true),
                    (vec!["tape", "identify"], true),
                    (vec!["help", "tape", "init"], saved && !system),
                    (vec!["tape", "help", "init"], saved && !system),
                    (vec!["tape", "--help"], saved && !system),
                    (
                        vec!["tape", "--config=daemon.toml", "init", "--help"],
                        false,
                    ),
                ] {
                    let args: Vec<OsString> = std::iter::once("rem")
                        .chain(words.iter().copied())
                        .map(OsString::from)
                        .collect();
                    assert_eq!(
                        selected_in(&args, &env),
                        expected,
                        "{args:?}, saved={saved}, system={system}"
                    );
                    if expected
                        && saved
                        && !system
                        && (words == ["put", "file"] || words == ["get", "object", "--out", "out"])
                    {
                        let test_env = Environment {
                            xdg: env.xdg.clone(),
                            system_config: env.system_config.clone(),
                            locks: env.locks.clone(),
                            test_input: true,
                            euid: 1000,
                            doctor_checks: None,
                        };
                        let mut out = Vec::new();
                        let mut errors = Vec::new();
                        let mut argv = args.clone();
                        argv.push("--allow-untrusted-state-volume".into());
                        assert_ne!(
                            run(
                                &argv,
                                test_env,
                                || Ok(remanence_library::sysfs::DeviceEnumeration {
                                    devices: vec![],
                                    warnings: vec![]
                                }),
                                &mut |_| panic!("fixture is empty"),
                                &mut out,
                                &mut errors
                            ),
                            ExitCode::SUCCESS
                        );
                        assert!(String::from_utf8_lossy(&errors).contains("saved drive DRIVE"));
                    }
                }
            }
        }
    }

    /// Missing defaults, root, and non-home setup refuse before any filesystem or hardware work.
    #[test]
    fn early_refusals_have_no_side_effects() {
        use super::*;
        let root = tempfile::tempdir().unwrap();
        for (words, euid, expected) in [
            (vec!["rem", "tape", "identify"], 1000, "requires --device"),
            (vec!["rem", "put", "file"], 1000, "requires --device"),
            (vec!["rem", "doctor"], 0, "not meant to run as root"),
            (
                vec!["rem", "setup", "--config", "missing"],
                1000,
                "setup saves a home default",
            ),
        ] {
            let env = Environment {
                xdg: root.path().join("state"),
                system_config: root.path().join("etc"),
                locks: root.path().join("locks"),
                test_input: true,
                euid,
                doctor_checks: None,
            };
            let mut out = Vec::new();
            let mut errors = Vec::new();
            let code = run(
                &words.iter().map(OsString::from).collect::<Vec<_>>(),
                env,
                || panic!("must not enumerate"),
                &mut |_| panic!("must not open hardware"),
                &mut out,
                &mut errors,
            );
            assert_ne!(code, ExitCode::SUCCESS);
            assert!(String::from_utf8_lossy(&errors).contains(expected));
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        }
    }

    /// All passing host checks plus a discovered standalone drive yield a successful doctor envelope.
    #[test]
    fn ready_doctor_exits_zero() {
        use super::*;
        use remanence_chaos::model::{DeviceRole, ModelTransport, VirtualWorld};
        let root = tempfile::tempdir().unwrap();
        let env = Environment {
            xdg: root.path().join("state"),
            system_config: root.path().join("etc"),
            locks: root.path().join("locks"),
            test_input: true,
            euid: 1000,
            doctor_checks: Some(json!({"checks":[{"name":"host", "ok":true}],"fixes":{}})),
        };
        let world = std::sync::Arc::new(std::sync::Mutex::new(VirtualWorld::single_drive(
            "LIB", 0x100, "DRIVE", 0x400, 0,
        )));
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let code = run(
            &["rem", "doctor", "--json"].map(OsString::from),
            env,
            || {
                Ok(remanence_library::sysfs::DeviceEnumeration {
                    devices: vec![DeviceAttachment {
                        sg_path: "/dev/fixture".into(),
                        sysfs_path: "/sys/fixture".into(),
                    }],
                    warnings: vec![],
                })
            },
            &mut |_| {
                Ok(Box::new(ModelTransport::new(
                    world.clone(),
                    DeviceRole::Drive { bay: 0x100 },
                )) as Box<dyn SgTransport>)
            },
            &mut out,
            &mut errors,
        );
        assert_eq!(code, ExitCode::SUCCESS, "{}", String::from_utf8_lossy(&out));
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["result"]["ready"], true);
        assert!(value.get("error").is_none());
    }

    /// Empty process values use HOME rather than a relative rem directory.
    #[test]
    fn empty_xdg_falls_back() {
        const CHILD: &str = "REM_EMPTY_XDG_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            assert_eq!(
                super::Environment::from_process().xdg,
                std::path::PathBuf::from("/home/test/.local/state")
            );
        } else {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "standalone::tests::empty_xdg_falls_back"])
                .env(CHILD, "1")
                .env("HOME", "/home/test")
                .env("XDG_STATE_HOME", "")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{} {}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }

    /// The minimal finalize grammar and its aliases share stable retry defaults.
    #[test]
    fn finalize_aliases_and_defaults() {
        use super::*;
        for ack in ["--acknowledge-tape", "--ack-tape-uuid"] {
            let cli = Cli::try_parse_from(["rem", "tape", "finalize", ack, "LABEL"]).unwrap();
            let Command::Tape {
                command:
                    Tape::Finalize {
                        tape,
                        acknowledge_tape,
                        reason,
                        idempotency_key,
                        expected_pool,
                    },
            } = cli.command
            else {
                panic!("finalize parser");
            };
            assert_eq!(acknowledge_tape, "LABEL");
            assert!(
                tape.is_none()
                    && reason.is_none()
                    && idempotency_key.is_none()
                    && expected_pool.is_none()
            );
        }
        for dest in ["--out", "--dest"] {
            assert!(Cli::try_parse_from(["rem", "get", "file", dest, "output"]).is_ok());
            assert!(crate::Cli::try_parse_from(["rem", "get", "file", dest, "output"]).is_ok());
        }
        for (tape, ack) in [
            ("--tape", "--acknowledge-tape"),
            ("--tape-uuid", "--ack-tape-uuid"),
        ] {
            let uuid = "00000000-0000-4000-8000-000000000001";
            let args = [
                "rem",
                "tape",
                "finalize",
                tape,
                uuid,
                ack,
                uuid,
                "--reason",
                "close",
                "--idempotency-key",
                uuid,
            ];
            assert!(Cli::try_parse_from(args).is_ok());
            assert!(crate::Cli::try_parse_from(args).is_ok());
        }
    }

    /// Root permission decisions happen before automatic home configuration writes.
    #[test]
    fn root_requires_explicit_configuration_in_home_mode() {
        assert!(super::refuse_root(false, false, true));
        assert!(!super::refuse_root(true, false, true));
        assert!(!super::refuse_root(false, true, true));
        assert!(!super::refuse_root(false, false, false));
    }

    /// Pin the namespace and byte encoding used for durable retry identities.
    #[test]
    fn finalize_key_golden() {
        assert_eq!(
            super::default_finalize_key([0; 16]).to_string(),
            "3bcca802-1fc9-57fd-9b2a-3926094e199f"
        );
    }

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
            euid: 1000,
            doctor_checks: None,
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
            euid: 1000,
            doctor_checks: None,
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
    fn shared_help_preserves_literal_operands_and_defers_command_help() {
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
            assert!(
                shared_help(&args).is_none(),
                "command help must reach the selected parser"
            );
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
