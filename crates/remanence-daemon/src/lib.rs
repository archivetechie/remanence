//! Layer 5 daemon server.
//!
//! Serves the same Daemon, Catalog, WriteSession, ReadSession, and Library
//! gRPC services over a local Unix-domain socket and, when configured, a TCP
//! listener protected by mutual TLS.

use std::future::Future;
use std::io;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::Path;
use std::time::Duration;

use remanence_api::{pb, ApiState};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::watch;
use tokio_stream::wrappers::{TcpListenerStream, UnixListenerStream};
use tokio_stream::StreamExt;
use tonic::transport::Server;

mod tls;
pub use tls::{load_server_tls, TlsConfigError, TlsListener};

const H2_INITIAL_STREAM_WINDOW_BYTES: u32 = 4 * 1024 * 1024;
const H2_INITIAL_CONNECTION_WINDOW_BYTES: u32 = 4 * 1024 * 1024;
const H2_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);
const H2_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct H2FlowControlConfig {
    initial_stream_window_bytes: u32,
    initial_connection_window_bytes: u32,
    keepalive_interval: Duration,
    keepalive_timeout: Duration,
}

fn h2_flow_control_config() -> H2FlowControlConfig {
    H2FlowControlConfig {
        initial_stream_window_bytes: H2_INITIAL_STREAM_WINDOW_BYTES,
        initial_connection_window_bytes: H2_INITIAL_CONNECTION_WINDOW_BYTES,
        keepalive_interval: H2_KEEPALIVE_INTERVAL,
        keepalive_timeout: H2_KEEPALIVE_TIMEOUT,
    }
}

fn server_builder() -> Server {
    let config = h2_flow_control_config();
    Server::builder()
        .initial_stream_window_size(config.initial_stream_window_bytes)
        .initial_connection_window_size(config.initial_connection_window_bytes)
        .http2_keepalive_interval(Some(config.keepalive_interval))
        .http2_keepalive_timeout(Some(config.keepalive_timeout))
}

/// Serve Layer 5 over the local Unix socket and optional TCP/mTLS listener.
///
/// A stale socket from a prior unclean exit is removed before bind; the socket
/// file is unlinked when this function exits.
pub async fn serve(
    state: ApiState,
    socket_path: &Path,
    tls_listener: Option<TlsListener>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ensure_socket_parent(socket_path)?;
    remove_stale_socket_before_bind(socket_path)?;
    let uds = UnixListener::bind(socket_path)?;
    harden_socket_permissions(socket_path)?;
    let tcp_listener = if let Some(listener) = tls_listener {
        match TcpListener::bind(listener.addr).await {
            Ok(tcp) => Some((tcp, listener.tls)),
            Err(err) => {
                let _ = std::fs::remove_file(socket_path);
                return Err(Box::new(err));
            }
        }
    } else {
        None
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let tcp_task_input = if let Some((tcp, tls)) = tcp_listener {
        let tcp_state = state.clone();
        let tcp_shutdown = shutdown_future(shutdown_rx.clone());
        let mut tcp_builder = match server_builder().tls_config(tls) {
            Ok(builder) => builder,
            Err(err) => {
                let _ = std::fs::remove_file(socket_path);
                return Err(Box::new(err));
            }
        };
        let tcp_router = tcp_builder
            .add_service(pb::daemon_server::DaemonServer::new(
                tcp_state.daemon_service(),
            ))
            .add_service(pb::catalog_server::CatalogServer::new(
                tcp_state.catalog_service(),
            ))
            .add_service(
                pb::write_session_service_server::WriteSessionServiceServer::new(
                    tcp_state.write_session_service(),
                ),
            )
            .add_service(
                pb::read_session_service_server::ReadSessionServiceServer::new(
                    tcp_state.read_session_service(),
                ),
            )
            .add_service(pb::audit_server::AuditServer::new(
                tcp_state.audit_service(),
            ))
            .add_service(pb::library_service_server::LibraryServiceServer::new(
                tcp_state.library_service(),
            ));
        Some((tcp, tcp_shutdown, tcp_router))
    } else {
        None
    };

    let unix_shutdown = shutdown_future(shutdown_rx);
    tokio::spawn(async move {
        shutdown.await;
        let _ = shutdown_tx.send(true);
    });

    let unix_state = state.clone();
    let mut unix_server = tokio::spawn(async move {
        let unix_incoming = UnixListenerStream::new(uds).filter_map(|accepted| match accepted {
            Ok(stream) if unix_peer_allowed(&stream) => Some(Ok(stream)),
            Ok(_) => None,
            Err(err) => Some(Err(err)),
        });
        server_builder()
            .add_service(pb::daemon_server::DaemonServer::new(
                unix_state.daemon_service(),
            ))
            .add_service(pb::catalog_server::CatalogServer::new(
                unix_state.catalog_service(),
            ))
            .add_service(
                pb::write_session_service_server::WriteSessionServiceServer::new(
                    unix_state.write_session_service(),
                ),
            )
            .add_service(
                pb::read_session_service_server::ReadSessionServiceServer::new(
                    unix_state.read_session_service(),
                ),
            )
            .add_service(pb::audit_server::AuditServer::new(
                unix_state.audit_service(),
            ))
            .add_service(pb::library_service_server::LibraryServiceServer::new(
                unix_state.library_service(),
            ))
            .serve_with_incoming_shutdown(unix_incoming, unix_shutdown)
            .await
    });

    let tcp_server = if let Some((tcp, tcp_shutdown, tcp_router)) = tcp_task_input {
        Some(tokio::spawn(async move {
            tcp_router
                .serve_with_incoming_shutdown(TcpListenerStream::new(tcp), tcp_shutdown)
                .await
        }))
    } else {
        None
    };

    let mut result: Result<(), Box<dyn std::error::Error + Send + Sync>> =
        if let Some(mut tcp_server) = tcp_server {
            tokio::select! {
                unix_join = &mut unix_server => match unix_join {
                    Ok(Ok(())) => match tcp_server.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(err)) => Err(Box::new(err)),
                        Err(err) => Err(Box::new(err)),
                    },
                    Ok(Err(err)) => {
                        tcp_server.abort();
                        Err(Box::new(err))
                    }
                    Err(err) => {
                        tcp_server.abort();
                        Err(Box::new(err))
                    }
                },
                tcp_join = &mut tcp_server => match tcp_join {
                    Ok(Ok(())) => match unix_server.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(err)) => Err(Box::new(err)),
                        Err(err) => Err(Box::new(err)),
                    },
                    Ok(Err(err)) => {
                        unix_server.abort();
                        Err(Box::new(err))
                    }
                    Err(err) => {
                        unix_server.abort();
                        Err(Box::new(err))
                    }
                },
            }
        } else {
            match unix_server.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(Box::new(err)),
                Err(err) => Err(Box::new(err)),
            }
        };

    if let Err(err) = state.shutdown_drive_pool().await {
        let message = match result {
            Ok(()) => format!("drive-pool shutdown dismount failed: {err}"),
            Err(server_err) => {
                format!("server failed: {server_err}; drive-pool shutdown dismount failed: {err}")
            }
        };
        result = Err(Box::new(io::Error::other(message)));
    }
    let _ = std::fs::remove_file(socket_path);
    result
}

fn remove_stale_socket_before_bind(
    socket_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if !socket_path.exists() {
        return Ok(());
    }

    match StdUnixStream::connect(socket_path) {
        Ok(_) => Err(Box::new(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "daemon socket {} already has a listening owner",
                socket_path.display()
            ),
        ))),
        Err(err) if matches!(err.kind(), io::ErrorKind::ConnectionRefused) => {
            std::fs::remove_file(socket_path)?;
            Ok(())
        }
        Err(err) if matches!(err.kind(), io::ErrorKind::NotFound) => Ok(()),
        Err(err) => Err(Box::new(io::Error::new(
            err.kind(),
            format!(
                "cannot verify daemon socket {}: {err}",
                socket_path.display()
            ),
        ))),
    }
}

async fn shutdown_future(mut rx: watch::Receiver<bool>) {
    if *rx.borrow() {
        return;
    }
    loop {
        if rx.changed().await.is_err() {
            return;
        }
        if *rx.borrow() {
            return;
        }
    }
}

fn unix_peer_allowed(stream: &UnixStream) -> bool {
    let Ok(cred) = stream.peer_cred() else {
        return false;
    };
    unix_peer_uid_allowed(cred.uid(), current_effective_uid())
}

fn unix_peer_uid_allowed(peer_uid: libc::uid_t, daemon_uid: libc::uid_t) -> bool {
    peer_uid == 0 || peer_uid == daemon_uid
}

fn current_effective_uid() -> libc::uid_t {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

fn ensure_socket_parent(
    socket_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(parent) = socket_path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(parent)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn harden_socket_permissions(
    socket_path: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(unix)]
    {
        let mut permissions = std::fs::metadata(socket_path)?.permissions();
        permissions.set_mode(0o660);
        std::fs::set_permissions(socket_path, permissions)?;
    }
    #[cfg(not(unix))]
    {
        let _ = socket_path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_peer_uid_policy_allows_root_and_daemon_user_only() {
        assert!(unix_peer_uid_allowed(0, 1000));
        assert!(unix_peer_uid_allowed(1000, 1000));
        assert!(!unix_peer_uid_allowed(1001, 1000));
    }

    #[test]
    fn daemon_server_builder_uses_windows_and_dead_peer_keepalive() {
        let config = h2_flow_control_config();
        assert_eq!(config.initial_stream_window_bytes, 4 * 1024 * 1024);
        assert_eq!(config.initial_connection_window_bytes, 4 * 1024 * 1024);
        assert_eq!(config.keepalive_interval, Duration::from_secs(30));
        assert_eq!(config.keepalive_timeout, Duration::from_secs(20));
        let _builder = server_builder();
    }
}
