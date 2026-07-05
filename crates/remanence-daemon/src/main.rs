//! rem-daemon — Layer 5 local daemon entrypoint.

use std::path::PathBuf;
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
        let spool_dir = config.daemon.state_dir.join("spool");
        if let Err(error) = create_private_spool_dir(&spool_dir) {
            eprintln!("error: create spool dir {}: {error}", spool_dir.display());
            return ExitCode::from(1);
        }
        match remanence_api::ApiState::with_drive_pool(index, &config, report, policy, spool_dir) {
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

fn create_private_spool_dir(path: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
