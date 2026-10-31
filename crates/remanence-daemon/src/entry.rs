//! Reusable Layer 5 daemon process entrypoint.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

/// Emit process diagnostics even when the controlling terminal closes.
macro_rules! stderr_message {
    ($($arg:tt)*) => {{
        let _ = writeln!(std::io::stderr().lock(), $($arg)*);
    }};
}

/// Keep tracing usable when stderr returns EIO or another write error.
struct BestEffortWriter<W>(W);

impl<W: Write> Write for BestEffortWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.write_all(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = self.0.flush();
        Ok(())
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "rem-daemon",
    version,
    about = "Remanence Layer 5 catalog daemon"
)]
struct Args {
    /// Path to the daemon config TOML.
    #[arg(long, value_name = "PATH", default_value = "/etc/rem/config.toml")]
    config: PathBuf,

    /// Override the listen socket path (default: config [daemon] socket_path,
    /// else <state_dir>/rem.sock).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
}

/// Install the graceful-shutdown signal handlers and return a future that
/// resolves when SIGINT, SIGTERM or SIGHUP arrives.
///
/// The graceful path closes write sessions and dismounts every seated
/// cartridge: SSC UNLOAD, then the changer move home. A process that dies
/// between those two commands leaves the cartridge unthreaded in the drive,
/// and the drive answers NOT READY / INITIALIZING COMMAND REQUIRED until a
/// host sends LOAD. SIGHUP is handled for that reason: a closing terminal or
/// multiplexer session sends it, and its default action ends the process at
/// once, wherever the dismount stands. Handled, it starts the same graceful
/// shutdown as SIGTERM.
///
/// The handlers stay installed for the life of the process, so a repeated
/// stop request (another SIGTERM, SIGINT or SIGHUP) does not cut a dismount
/// short; the daemon exits once the shutdown pass has finished every move it
/// started. SIGKILL or a failed changer move can leave a cartridge unthreaded, so a
/// supervisor's stop timeout must cover a rewind, an unload and a changer
/// move, which together can take more than a minute on LTO-9.
fn shutdown_signal() -> std::io::Result<impl std::future::Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    Ok(report_shutdown_signal(async move {
        tokio::select! {
            _ = interrupt.recv() => "SIGINT",
            _ = terminate.recv() => "SIGTERM",
            _ = hangup.recv() => "SIGHUP",
        }
    }))
}

/// Complete the shutdown notification even when its diagnostic cannot be written.
async fn report_shutdown_signal(signal: impl std::future::Future<Output = &'static str>) {
    let name = signal.await;
    stderr_message!(
        "rem-daemon: {name} received; closing sessions and finishing every cartridge dismount before exit"
    );
}

/// Run the daemon with the foreign formats linked by a distribution.
#[tokio::main]
pub async fn main_entry_with_registry(
    foreign_formats: remanence_format_driver::ForeignFormatRegistry,
) -> ExitCode {
    init_tracing();
    tracing::info!(
        kernel = remanence_parity::codec::encoding_kernel(),
        "parity encoding kernel"
    );
    let args = Args::parse();

    let config = match remanence_state::load_config(&args.config) {
        Ok(config) => config,
        Err(error) => {
            stderr_message!("error: load config {}: {error}", args.config.display());
            return ExitCode::from(1);
        }
    };
    warn_io_memory_sanity(config.daemon.io_memory_ceiling);

    // The state lock is a process-lifetime ownership boundary, not merely an
    // offline-maintenance mutex. It prevents a second daemon on another socket
    // (or a direct state mutator) from racing SQLite, checkpoint recovery, or
    // the hash-chained audit append sequence.
    let _state_lock = match remanence_state::StateLockGuard::acquire(&config.daemon.state_dir) {
        Ok(lock) => lock,
        Err(error) => {
            stderr_message!(
                "error: acquire daemon state ownership {}: {error}",
                config.daemon.state_dir.display()
            );
            return ExitCode::from(1);
        }
    };

    if let Err(error) = remanence_state::authority_layout::admit_owned_config(&config) {
        stderr_message!("error: authority layout: {error}");
        return ExitCode::from(1);
    }

    let socket_path = args
        .socket
        .unwrap_or_else(|| config.daemon.socket_path_or_default());

    let index = match remanence_state::CatalogIndex::open(&config.index.sqlite_path) {
        Ok(index) => index,
        Err(error) => {
            stderr_message!(
                "error: open index {}: {error}",
                config.index.sqlite_path.display()
            );
            return ExitCode::from(1);
        }
    };
    let state = if config.daemon.read_only {
        match remanence_api::ApiState::new_with_config(index, &config) {
            Ok(state) => state,
            Err(error) => {
                stderr_message!("error: replay checkpoint journals: {error}");
                return ExitCode::from(1);
            }
        }
    } else {
        let report = match remanence_library::discover() {
            Ok(report) => report,
            Err(error) => {
                stderr_message!("error: discover libraries: {error}");
                return ExitCode::from(1);
            }
        };
        for warning in &report.warnings {
            if let Some(message) = remanence_library::drive_mapping_message(warning)
                .or_else(|| warning.informational_message())
            {
                stderr_message!("rem-daemon: {message}");
            }
        }
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
            stderr_message!("error: create spool dir {}: {error}", spool_dir.display());
            return ExitCode::from(1);
        }
        let reconciliation = match reconcile_spool_orphans(&spool_dir) {
            Ok(evidence) => evidence,
            Err(error) => {
                stderr_message!(
                    "error: reconcile append spool {} before accepting writes: {error}",
                    spool_dir.display()
                );
                return ExitCode::from(1);
            }
        };
        if !reconciliation.is_empty() {
            stderr_message!(
                "rem-daemon: append spool orphan reconciliation spool_dir={} removed_count={} removed_bytes={} evidence={}",
                spool_dir.display(),
                reconciliation.len(),
                reconciliation
                    .iter()
                    .fold(0u64, |total, item| total.saturating_add(item.size_bytes)),
                format_spool_orphan_evidence(&reconciliation),
            );
        }
        let spool_budget = match resolve_spool_budget(&config.daemon, &spool_dir) {
            Ok(budget) => budget,
            Err(error) => {
                stderr_message!(
                    "error: configure spool dir {}: {error}",
                    spool_dir.display()
                );
                return ExitCode::from(1);
            }
        };
        if spool_budget.is_tmpfs {
            stderr_message!(
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
                stderr_message!("error: start drive pool: {error}");
                return ExitCode::from(1);
            }
        }
    };

    let state = state.with_foreign_formats(foreign_formats);

    let tls_listener = match (&config.daemon.listen, &config.daemon.tls) {
        (Some(listen), Some(tls)) => {
            let addr = match listen.parse() {
                Ok(addr) => addr,
                Err(error) => {
                    stderr_message!("error: parse daemon.listen {listen:?}: {error}");
                    return ExitCode::from(1);
                }
            };
            let tls = match crate::load_server_tls(tls) {
                Ok(tls) => tls,
                Err(error) => {
                    stderr_message!("error: load daemon TLS material: {error}");
                    return ExitCode::from(1);
                }
            };
            Some(crate::TlsListener { addr, tls })
        }
        _ => None,
    };

    if let Some(listener) = &tls_listener {
        stderr_message!(
            "rem-daemon: configured mTLS listener on tcp:{}",
            listener.addr
        );
    }
    stderr_message!(
        "rem-daemon: serving local Layer 5 API on unix:{}",
        socket_path.display()
    );
    let shutdown = match shutdown_signal() {
        Ok(shutdown) => shutdown,
        Err(error) => {
            stderr_message!("error: install shutdown signal handlers: {error}");
            return ExitCode::from(1);
        }
    };
    match crate::serve(state, &socket_path, tls_listener, shutdown).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            stderr_message!("error: serve: {error}");
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(|| BestEffortWriter(std::io::stderr()))
        .json()
        .flatten_event(true)
        .try_init();
}

#[cfg(test)]
mod signal_tests {
    use super::*;

    /// Complete the shutdown notification with a closed pty master in a child process.
    #[test]
    fn closed_stderr_does_not_panic_shutdown() {
        const CHILD: &str = "REM_TEST_CLOSED_STDERR";
        if std::env::var_os(CHILD).is_some() {
            let runtime = tokio::runtime::Runtime::new().expect("runtime");
            runtime.block_on(async {
                // Supply the notification directly; signal delivery has separate coverage.
                let shutdown = report_shutdown_signal(std::future::ready("SIGHUP"));
                let (mut master, mut slave) = (-1, -1);
                // SAFETY: both output pointers are valid; null selects default pty settings.
                assert_eq!(
                    unsafe {
                        libc::openpty(
                            &mut master,
                            &mut slave,
                            std::ptr::null_mut(),
                            std::ptr::null(),
                            std::ptr::null(),
                        )
                    },
                    0,
                    "open terminal pair"
                );
                // SAFETY: this isolated child owns these descriptors. Closing the master
                // hangs up stderr's slave without changing the parent test's terminal.
                unsafe {
                    assert_eq!(libc::dup2(slave, 2), 2);
                    assert_eq!(libc::close(slave), 0);
                    assert_eq!(libc::close(master), 0);
                }
                let error = std::io::stderr()
                    .write_all(b"terminal probe")
                    .expect_err("pty is closed");
                assert_eq!(error.raw_os_error(), Some(libc::EIO));
                init_tracing();
                tracing::error!("cartridge stays unthreaded in the drive");
                stderr_message!("error: cartridge stays unthreaded in the drive");
                tokio::time::timeout(std::time::Duration::from_secs(5), shutdown)
                    .await
                    .expect("shutdown resolves with closed stderr");
            });
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "entry::signal_tests::closed_stderr_does_not_panic_shutdown",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("run isolated shutdown test");
        assert!(output.status.success(), "child fails: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "child runs the shutdown test: {output:?}"
        );
    }

    /// A hung-up pty reports EIO rather than a short successful write.
    #[test]
    fn tracing_writer_tolerates_terminal_eio() {
        struct HungUp;
        impl Write for HungUp {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::from_raw_os_error(libc::EIO))
            }
        }
        let mut writer = BestEffortWriter(HungUp);
        writer.write_all(b"shutdown error").expect("ignore EIO");
        writer.flush().expect("ignore flush EIO");
        let subscriber = tracing_subscriber::fmt()
            .with_writer(|| BestEffortWriter(HungUp))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("cartridge stays unthreaded in the drive");
        });
    }

    #[tokio::test]
    async fn sighup_requests_the_graceful_shutdown() {
        let shutdown = shutdown_signal().expect("install shutdown signal handlers");
        // SAFETY: raise has no memory-safety preconditions. The handlers
        // installed above replace the default SIGHUP action for the rest of
        // the process, so the signal reaches the shutdown future instead of
        // ending the test process.
        let raised = unsafe { libc::raise(libc::SIGHUP) };
        assert_eq!(raised, 0, "raise SIGHUP");
        tokio::time::timeout(std::time::Duration::from_secs(5), shutdown)
            .await
            .expect("SIGHUP resolves the graceful shutdown future");
    }
}

#[cfg(test)]
mod argument_tests {
    use super::*;
    use clap::error::ErrorKind;

    #[test]
    fn version_flag_exits_without_loading_configuration() {
        let error = Args::try_parse_from(["rem-daemon", "--version"])
            .expect_err("--version exits after printing the package version");
        assert_eq!(error.kind(), ErrorKind::DisplayVersion);
        assert!(
            error.to_string().contains(env!("CARGO_PKG_VERSION")),
            "version output must contain the package version: {error}"
        );
    }
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

#[derive(Debug, PartialEq, Eq)]
struct SpoolOrphanEvidence {
    name: String,
    size_bytes: u64,
    modified_unix_seconds: Option<u64>,
}

/// Remove only daemon-owned UUID-named append spools left by an unclean exit.
/// This runs before budget discovery so tmpfs capacity is re-accounted after
/// orphan removal and before the API can accept a write stream.
fn reconcile_spool_orphans(path: &Path) -> std::io::Result<Vec<SpoolOrphanEvidence>> {
    let mut evidence = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(uuid_text) = name
            .strip_prefix("spool-")
            .and_then(|rest| rest.strip_suffix(".bin"))
        else {
            continue;
        };
        if !is_canonical_uuid(uuid_text) {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified_unix_seconds = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        evidence.push(SpoolOrphanEvidence {
            name: name.to_string(),
            size_bytes: metadata.len(),
            modified_unix_seconds,
        });
    }
    evidence.sort_by(|left, right| left.name.cmp(&right.name));
    if !evidence.is_empty() {
        stderr_message!(
            "rem-daemon: append spool orphan evidence spool_dir={} orphan_count={} orphan_bytes={} action=remove_before_budget_reaccount evidence={}",
            path.display(),
            evidence.len(),
            evidence
                .iter()
                .fold(0u64, |total, item| total.saturating_add(item.size_bytes)),
            format_spool_orphan_evidence(&evidence),
        );
    }
    for item in &evidence {
        std::fs::remove_file(path.join(&item.name))?;
    }
    Ok(evidence)
}

fn is_canonical_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'),
        })
}

fn format_spool_orphan_evidence(evidence: &[SpoolOrphanEvidence]) -> String {
    evidence
        .iter()
        .map(|item| {
            format!(
                "{}:{}:{}",
                item.name,
                item.size_bytes,
                item.modified_unix_seconds
                    .map(|seconds| seconds.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )
        })
        .collect::<Vec<_>>()
        .join(",")
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
        config.io_memory_ceiling,
        info,
    )
    .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))
}

fn effective_spool_budget(
    default_bytes: u64,
    configured_tmpfs_budget: Option<u64>,
    io_memory_ceiling: u64,
    info: SpoolFilesystemInfo,
) -> Result<SpoolBudget, String> {
    if !info.is_tmpfs {
        return Ok(SpoolBudget {
            effective_bytes: default_bytes.min(io_memory_ceiling),
            available_bytes: info.available_bytes,
            is_tmpfs: false,
        });
    }
    let acknowledged = configured_tmpfs_budget.ok_or_else(|| {
        "daemon.spool_tmpfs_ram_budget is required when daemon.spool_dir resolves to tmpfs"
            .to_string()
    })?;
    let effective = default_bytes.min(acknowledged).min(io_memory_ceiling);
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

fn warn_io_memory_sanity(ceiling: u64) {
    #[cfg(target_os = "linux")]
    {
        if let Some(available) = read_mem_available_bytes() {
            if ceiling > available {
                stderr_message!(
                    "rem-daemon: warning daemon.io_memory_ceiling={ceiling} exceeds startup MemAvailable={available}; the fixed ceiling remains authoritative"
                );
            }
        }
    }
    #[cfg(unix)]
    {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` points to writable storage of the exact type required
        // by `getrlimit`; the call does not retain the pointer.
        if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) } == 0
            && limit.rlim_cur != libc::RLIM_INFINITY
            && ceiling > limit.rlim_cur
        {
            stderr_message!(
                "rem-daemon: warning daemon.io_memory_ceiling={ceiling} exceeds LimitMEMLOCK={}; reads fall back to unlocked buffers when minimum read reservoirs cannot be locked",
                limit.rlim_cur
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_reconciliation_removes_owned_orphans_and_preserves_foreign_files() {
        let temp = tempfile::Builder::new()
            .prefix("rem-daemon-spool-reconcile")
            .tempdir()
            .expect("tempdir");
        let owned_name = "spool-01234567-89ab-cdef-0123-456789abcdef.bin";
        let owned = temp.path().join(owned_name);
        std::fs::write(&owned, b"orphan payload").expect("write owned orphan");
        let foreign_names = [
            "spool-not-a-uuid.bin",
            "spool-01234567-89AB-CDEF-0123-456789ABCDEF.bin",
            "spool-01234567-89ab-cdef-0123-456789abcdef.bin.keep",
            "operator-notes.txt",
        ];
        for name in foreign_names {
            std::fs::write(temp.path().join(name), b"foreign").expect("write foreign file");
        }

        let evidence = reconcile_spool_orphans(temp.path()).expect("reconcile orphans");

        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].name, owned_name);
        assert_eq!(evidence[0].size_bytes, 14);
        assert!(!owned.exists(), "owned orphan must be removed");
        for name in foreign_names {
            assert!(
                temp.path().join(name).exists(),
                "foreign file {name} must be preserved"
            );
        }
        let formatted = format_spool_orphan_evidence(&evidence);
        assert!(formatted.contains(owned_name));
        assert!(formatted.contains(":14:"));
    }

    #[cfg(unix)]
    #[test]
    fn startup_reconciliation_does_not_follow_owned_named_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::Builder::new()
            .prefix("rem-daemon-spool-reconcile-link")
            .tempdir()
            .expect("tempdir");
        let target = temp.path().join("foreign-target");
        std::fs::write(&target, b"foreign").expect("write target");
        let link = temp
            .path()
            .join("spool-01234567-89ab-cdef-0123-456789abcdef.bin");
        symlink(&target, &link).expect("create symlink");

        let evidence = reconcile_spool_orphans(temp.path()).expect("reconcile orphans");

        assert!(evidence.is_empty());
        assert!(link.exists(), "foreign symlink must be untouched");
        assert_eq!(std::fs::read(target).expect("read target"), b"foreign");
    }

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
    fn tmpfs_spool_budget_requires_acknowledgment_but_not_memavailable_authority() {
        let info = SpoolFilesystemInfo {
            is_tmpfs: true,
            available_bytes: 2 * 1024 * 1024,
        };
        let err = effective_spool_budget(64 * 1024 * 1024, None, 32 * 1024 * 1024, info)
            .expect_err("tmpfs budget ack required");
        assert!(err.contains("daemon.spool_tmpfs_ram_budget"), "{err}");

        let budget = effective_spool_budget(
            64 * 1024 * 1024,
            Some(8 * 1024 * 1024),
            32 * 1024 * 1024,
            info,
        )
        .expect("budget");
        assert_eq!(budget.effective_bytes, 8 * 1024 * 1024);
        assert_eq!(budget.available_bytes, 2 * 1024 * 1024);
        assert!(budget.is_tmpfs);
    }

    #[test]
    fn non_tmpfs_spool_budget_uses_default_without_acknowledgment() {
        let budget = effective_spool_budget(
            64 * 1024 * 1024,
            None,
            32 * 1024 * 1024,
            SpoolFilesystemInfo {
                is_tmpfs: false,
                available_bytes: 512,
            },
        )
        .expect("non-tmpfs budget");
        assert_eq!(budget.effective_bytes, 32 * 1024 * 1024);
        assert!(!budget.is_tmpfs);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_mem_available_from_proc_meminfo() {
        let text = "MemTotal: 1000 kB\nMemAvailable: 12345 kB\n";
        assert_eq!(parse_mem_available_bytes(text), Some(12_641_280));
    }
}
