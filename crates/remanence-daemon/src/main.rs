//! rem-daemon — Layer 5 local daemon entrypoint.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "rem-daemon", about = "Remanence Layer 5 catalog daemon")]
struct Args {
    /// Path to the daemon config TOML.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,

    /// Override the listen socket path (default: config [daemon] socket_path,
    /// else <state_dir>/rem.sock).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

/// Resolve when SIGINT or SIGTERM arrives.
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let args = Args::parse();

    let config = match remanence_state::load_config(&args.config) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: load config {}: {error}", args.config.display());
            return ExitCode::from(1);
        }
    };

    let socket_path = args
        .socket
        .unwrap_or_else(|| config.daemon.socket_path_or_default());

    let index = match remanence_state::CatalogIndex::open(&config.index.sqlite_path) {
        Ok(index) => index,
        Err(error) => {
            eprintln!(
                "error: open index {}: {error}",
                config.index.sqlite_path.display()
            );
            return ExitCode::from(1);
        }
    };
    let state = if config.daemon.read_only {
        remanence_api::ApiState::new_with_config(index, &config)
    } else {
        let report = match remanence_library::discover() {
            Ok(report) => report,
            Err(error) => {
                eprintln!("error: discover libraries: {error}");
                return ExitCode::from(1);
            }
        };
        let mut policy = remanence_library::StaticAllowlist::new(
            config.libraries.iter().map(|l| l.serial.clone()),
        );
        for library in &config.libraries {
            if library.allow_derived_drive_identity {
                policy = policy.with_derived_allowed(library.serial.clone());
            }
        }
        let spool_dir = config.daemon.spool_dir_or_default();
        if let Err(error) = create_private_spool_dir(&spool_dir) {
            eprintln!("error: create spool dir {}: {error}", spool_dir.display());
            return ExitCode::from(1);
        }
        let spool_budget = match resolve_spool_budget(&config.daemon, &spool_dir) {
            Ok(budget) => budget,
            Err(error) => {
                eprintln!(
                    "error: configure spool dir {}: {error}",
                    spool_dir.display()
                );
                return ExitCode::from(1);
            }
        };
        if spool_budget.is_tmpfs {
            eprintln!(
                "rem-daemon: tmpfs append spool {} effective_budget_bytes={} available_ram_budget_bytes={}",
                spool_dir.display(),
                spool_budget.effective_bytes,
                spool_budget.available_bytes,
            );
        }
        match remanence_api::ApiState::with_drive_pool(
            index,
            &config,
            report,
            policy,
            spool_dir,
            spool_budget.effective_bytes,
        ) {
            Ok(state) => state,
            Err(error) => {
                eprintln!("error: start drive pool: {error}");
                return ExitCode::from(1);
            }
        }
    };

    let tls_listener = match (&config.daemon.listen, &config.daemon.tls) {
        (Some(listen), Some(tls)) => {
            let addr = match listen.parse() {
                Ok(addr) => addr,
                Err(error) => {
                    eprintln!("error: parse daemon.listen {listen:?}: {error}");
                    return ExitCode::from(1);
                }
            };
            let tls = match remanence_daemon::load_server_tls(tls) {
                Ok(tls) => tls,
                Err(error) => {
                    eprintln!("error: load daemon TLS material: {error}");
                    return ExitCode::from(1);
                }
            };
            Some(remanence_daemon::TlsListener { addr, tls })
        }
        _ => None,
    };

    if let Some(listener) = &tls_listener {
        eprintln!(
            "rem-daemon: configured mTLS listener on tcp:{}",
            listener.addr
        );
    }
    eprintln!(
        "rem-daemon: serving local Layer 5 API on unix:{}",
        socket_path.display()
    );
    match remanence_daemon::serve(state, &socket_path, tls_listener, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: serve: {error}");
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .json()
        .flatten_event(true)
        .try_init();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpoolBudget {
    effective_bytes: u64,
    available_bytes: u64,
    is_tmpfs: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpoolFilesystemInfo {
    is_tmpfs: bool,
    available_bytes: u64,
}

fn create_private_spool_dir(path: &Path) -> std::io::Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(path)?;
            let resolved = resolve_symlink_target(path, &target);
            let target_metadata = std::fs::metadata(&resolved).map_err(|err| {
                std::io::Error::new(
                    err.kind(),
                    format!(
                        "spool dir symlink {} -> {} is dangling or inaccessible: {err}",
                        path.display(),
                        target.display()
                    ),
                )
            })?;
            if !target_metadata.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "spool dir symlink {} -> {} targets a non-directory",
                        path.display(),
                        target.display()
                    ),
                ));
            }
        }
    }
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn resolve_symlink_target(link: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target.to_path_buf()
    } else {
        link.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join(target)
    }
}

fn resolve_spool_budget(
    config: &remanence_state::DaemonConfig,
    spool_dir: &Path,
) -> std::io::Result<SpoolBudget> {
    let info = spool_filesystem_info(spool_dir)?;
    effective_spool_budget(
        remanence_api::APPEND_SPOOL_MAX_BYTES,
        config.spool_tmpfs_ram_budget,
        info,
    )
    .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))
}

fn effective_spool_budget(
    default_bytes: u64,
    configured_tmpfs_budget: Option<u64>,
    info: SpoolFilesystemInfo,
) -> Result<SpoolBudget, String> {
    if !info.is_tmpfs {
        return Ok(SpoolBudget {
            effective_bytes: default_bytes,
            available_bytes: info.available_bytes,
            is_tmpfs: false,
        });
    }
    let acknowledged = configured_tmpfs_budget.ok_or_else(|| {
        "daemon.spool_tmpfs_ram_budget is required when daemon.spool_dir resolves to tmpfs"
            .to_string()
    })?;
    let effective = default_bytes.min(acknowledged).min(info.available_bytes);
    if effective == 0 {
        return Err(
            "daemon.spool_dir resolves to tmpfs but no RAM budget is currently available"
                .to_string(),
        );
    }
    Ok(SpoolBudget {
        effective_bytes: effective,
        available_bytes: info.available_bytes,
        is_tmpfs: true,
    })
}

#[cfg(target_os = "linux")]
fn spool_filesystem_info(path: &Path) -> std::io::Result<SpoolFilesystemInfo> {
    let stats = nix::sys::statfs::statfs(path).map_err(std::io::Error::from)?;
    let fs_type = stats.filesystem_type().0 as u64;
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    const RAMFS_MAGIC: u64 = 0x8584_58F6;
    let is_tmpfs = matches!(fs_type, TMPFS_MAGIC | RAMFS_MAGIC);
    let fs_available = (stats.blocks_available() as u128)
        .saturating_mul(stats.block_size() as u128)
        .min(u128::from(u64::MAX)) as u64;
    let available_bytes = if is_tmpfs {
        read_mem_available_bytes()
            .map(|mem_available| mem_available.min(fs_available))
            .unwrap_or(fs_available)
    } else {
        fs_available
    };
    Ok(SpoolFilesystemInfo {
        is_tmpfs,
        available_bytes,
    })
}

#[cfg(not(target_os = "linux"))]
fn spool_filesystem_info(_path: &Path) -> std::io::Result<SpoolFilesystemInfo> {
    Ok(SpoolFilesystemInfo {
        is_tmpfs: false,
        available_bytes: remanence_api::APPEND_SPOOL_MAX_BYTES,
    })
}

#[cfg(target_os = "linux")]
fn read_mem_available_bytes() -> Option<u64> {
    parse_mem_available_bytes(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(target_os = "linux")]
fn parse_mem_available_bytes(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.trim_start().starts_with("MemAvailable:"))?;
    let mut parts = line.split_whitespace();
    let _label = parts.next()?;
    let kb = parts.next()?.parse::<u64>().ok()?;
    kb.checked_mul(1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn create_private_spool_dir_reports_dangling_symlink_target() {
        let temp = tempfile::Builder::new()
            .prefix("rem-daemon-spool-link")
            .tempdir()
            .expect("tempdir");
        let link = temp.path().join("spool");
        let target = Path::new("missing-target");
        std::os::unix::fs::symlink(target, &link).expect("symlink");

        let err = create_private_spool_dir(&link).expect_err("dangling symlink must fail");
        let message = err.to_string();
        assert!(message.contains("dangling"), "{message}");
        assert!(message.contains(&link.display().to_string()), "{message}");
        assert!(message.contains(&target.display().to_string()), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn create_private_spool_dir_accepts_valid_symlink_target() {
        let temp = tempfile::Builder::new()
            .prefix("rem-daemon-spool-link-ok")
            .tempdir()
            .expect("tempdir");
        let target = temp.path().join("ram-spool");
        std::fs::create_dir(&target).expect("target dir");
        let link = temp.path().join("spool");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        create_private_spool_dir(&link).expect("valid symlink target works");
        assert!(target.is_dir());
    }

    #[test]
    fn tmpfs_spool_budget_requires_acknowledgment_and_clamps_to_available_ram() {
        let info = SpoolFilesystemInfo {
            is_tmpfs: true,
            available_bytes: 2 * 1024 * 1024,
        };
        let err = effective_spool_budget(64 * 1024 * 1024, None, info)
            .expect_err("tmpfs budget ack required");
        assert!(err.contains("daemon.spool_tmpfs_ram_budget"), "{err}");

        let budget =
            effective_spool_budget(64 * 1024 * 1024, Some(8 * 1024 * 1024), info).expect("budget");
        assert_eq!(budget.effective_bytes, 2 * 1024 * 1024);
        assert_eq!(budget.available_bytes, 2 * 1024 * 1024);
        assert!(budget.is_tmpfs);
    }

    #[test]
    fn non_tmpfs_spool_budget_uses_default_without_acknowledgment() {
        let budget = effective_spool_budget(
            64 * 1024 * 1024,
            None,
            SpoolFilesystemInfo {
                is_tmpfs: false,
                available_bytes: 512,
            },
        )
        .expect("non-tmpfs budget");
        assert_eq!(budget.effective_bytes, 64 * 1024 * 1024);
        assert!(!budget.is_tmpfs);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_mem_available_from_proc_meminfo() {
        let text = "MemTotal: 1000 kB\nMemAvailable: 12345 kB\n";
        assert_eq!(parse_mem_available_bytes(text), Some(12_641_280));
    }
}
