# Layer 5 S2 — Network transport hardening (mTLS over TCP) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve the Layer 5 daemon over a TCP listener with mutual TLS, concurrently with the existing local Unix socket, configured via `[daemon] listen` + `[daemon.tls]`.

**Architecture:** New `DaemonConfig.listen` + `DaemonConfig.tls` config. A daemon-crate `tls` module loads operator PEMs into a `tonic` `ServerTlsConfig` (client cert required). `serve()` runs the Unix server and a TCP/mTLS server concurrently as two `tokio::spawn`ed tasks sharing one `Notify`-based shutdown. `read_only` keeps its existing implicit (no-owner) enforcement.

**Tech Stack:** Rust, tonic 0.14.5 `tls-ring` (rustls/ring), `tokio`, `remanence-state` config.

---

## Context the implementer needs

- **Design doc:** `docs/layer5-s2-network-mtls-design-v0.1.md` (read first — trust model, the all-or-nothing config rule, the two-server shutdown).
- **tonic 0.14.5 TLS (verified 2026-06-04):** with the `tls-ring` feature, `tonic::transport::{Server, ServerTlsConfig, Identity, Certificate}` are available. `ServerTlsConfig::new().identity(Identity::from_pem(cert, key)).client_ca_root(Certificate::from_pem(client_ca))` requires a client cert (mTLS). `Server::tls_config(cfg) -> Result<Self, tonic::transport::Error>` is on `Server` (call **before** `add_service`). `Router::serve_with_shutdown(addr, signal)` and `Router::serve_with_incoming_shutdown(incoming, signal)` exist. `Identity::from_pem`/`Certificate::from_pem` store bytes (no parse error at construction).
- **Current `serve()`** (`crates/remanence-daemon/src/lib.rs`): one `UnixListener`, `Server::builder().add_service(×5: daemon/catalog/write_session/read_session/library).serve_with_incoming_shutdown(uds, shutdown)`, removes stale socket before bind + unlinks on exit. Signature: `serve(state: ApiState, socket_path: &Path, shutdown: impl Future<Output=()> + Send + 'static) -> Result<(), Box<dyn Error + Send + Sync>>`.
- **Current `main`** (`crates/remanence-daemon/src/main.rs`): loads config → resolves `socket_path` → opens index → builds `state` (`read_only` → `new_with_config`, else discover + `with_session_owner`) → `serve(state, &socket_path, shutdown_signal()).await`. Config-load/discover failures `eprintln!` + `return ExitCode::from(1)`.
- **`DaemonConfig`** (`crates/remanence-state/src/config.rs`, `#[serde(deny_unknown_fields)]`): `state_dir`, `default_idle_timeout_seconds`, `read_only` (`#[serde(default)]`), `socket_path: Option<PathBuf>` (`#[serde(default)]`). `validate_config` calls `require_absolute("daemon.state_dir", …)` and (when present) `require_absolute("daemon.socket_path", …)`.
- **Existing test:** `crates/remanence-daemon/tests/serve_catalog.rs` calls `serve(state, &socket_path, async {…})` — its `serve(...)` call must gain the new `None` argument.

## File Structure

- **Modify** `crates/remanence-state/src/config.rs` — `DaemonTlsConfig` struct; `listen` + `tls` fields on `DaemonConfig`; validation; tests.
- **Modify** `crates/remanence-daemon/Cargo.toml` — `tonic` `tls-ring` feature.
- **Create** `crates/remanence-daemon/src/tls.rs` — `TlsConfigError`, `TlsListener`, `load_server_tls`.
- **Modify** `crates/remanence-daemon/src/lib.rs` — `mod tls; pub use`; `serve()` signature + dual-server body.
- **Modify** `crates/remanence-daemon/src/main.rs` — build `Option<TlsListener>` from config; pass to `serve()`.
- **Modify** `crates/remanence-daemon/tests/serve_catalog.rs` — pass `None` for the new arg.

---

## Task 1: Config — `listen` + `tls`

**Files:**
- Modify: `crates/remanence-state/src/config.rs`

- [ ] **Step 1: Add the `DaemonTlsConfig` struct and the two fields**

Add the struct near `DaemonConfig`:
```rust
/// Server-side mutual-TLS material for the daemon's TCP listener. All paths are
/// operator-provisioned PEM files; absolute (validated). Setting this (with
/// `listen`) enables mTLS over TCP alongside the Unix socket.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DaemonTlsConfig {
    /// Server identity certificate (PEM).
    pub cert: PathBuf,
    /// Server private key (PEM).
    pub key: PathBuf,
    /// CA whose signature a client certificate must carry (PEM).
    pub client_ca: PathBuf,
}
```
Add to `DaemonConfig` (after `socket_path`):
```rust
    /// TCP listen address for the mTLS endpoint, e.g. "0.0.0.0:8443". Requires
    /// `tls`. Absent → no TCP listener (Unix socket only).
    #[serde(default)]
    pub listen: Option<String>,
    /// Mutual-TLS material for the TCP listener. Requires `listen`.
    #[serde(default)]
    pub tls: Option<DaemonTlsConfig>,
```

- [ ] **Step 2: Add validation**

In `validate_config`, after the `socket_path` `require_absolute` block, add:
```rust
    match (&config.daemon.listen, &config.daemon.tls) {
        (Some(listen), Some(tls)) => {
            listen.parse::<std::net::SocketAddr>().map_err(|_| {
                StateError::ConfigInvalid(format!(
                    "daemon.listen {listen:?} must be a valid socket address (e.g. 0.0.0.0:8443)"
                ))
            })?;
            require_absolute("daemon.tls.cert", &tls.cert)?;
            require_absolute("daemon.tls.key", &tls.key)?;
            require_absolute("daemon.tls.client_ca", &tls.client_ca)?;
        }
        (None, None) => {}
        _ => {
            return Err(StateError::ConfigInvalid(
                "daemon.listen and daemon.tls must be set together".to_string(),
            ));
        }
    }
```
(`require_absolute` already returns `StateError::ConfigInvalid(String)` — the same variant used here.)

- [ ] **Step 3: Write tests**

Add to the config `mod tests`:
```rust
    #[test]
    fn daemon_listen_and_tls_parse_together() {
        let text = format!(
            "{}\nlisten = \"0.0.0.0:8443\"\n\n[daemon.tls]\ncert = \"/etc/rem/s.crt\"\nkey = \"/etc/rem/s.key\"\nclient_ca = \"/etc/rem/ca.crt\"\n",
            valid_config()
        );
        let config = parse_config_toml(&text).expect("valid config with listen+tls");
        assert_eq!(config.daemon.listen.as_deref(), Some("0.0.0.0:8443"));
        assert_eq!(
            config.daemon.tls.as_ref().unwrap().client_ca,
            std::path::PathBuf::from("/etc/rem/ca.crt")
        );
    }

    #[test]
    fn daemon_listen_without_tls_is_rejected() {
        let text = format!("{}\nlisten = \"0.0.0.0:8443\"\n", valid_config());
        let err = parse_config_toml(&text).expect_err("listen without tls");
        assert!(err.to_string().contains("must be set together"), "{err}");
    }

    #[test]
    fn daemon_unparseable_listen_is_rejected() {
        let text = format!(
            "{}\nlisten = \"not-an-addr\"\n\n[daemon.tls]\ncert = \"/c\"\nkey = \"/k\"\nclient_ca = \"/ca\"\n",
            valid_config()
        );
        let err = parse_config_toml(&text).expect_err("bad listen");
        assert!(err.to_string().contains("daemon.listen"), "{err}");
    }

    #[test]
    fn daemon_relative_tls_path_is_rejected() {
        let text = format!(
            "{}\nlisten = \"0.0.0.0:8443\"\n\n[daemon.tls]\ncert = \"rel/s.crt\"\nkey = \"/k\"\nclient_ca = \"/ca\"\n",
            valid_config()
        );
        let err = parse_config_toml(&text).expect_err("relative cert");
        assert!(err.to_string().contains("daemon.tls.cert"), "{err}");
    }
```
(Use the existing test helpers `valid_config()` / `parse_config_toml`; confirm their names. If `valid_config()` already contains a `[daemon.tls]` table or a `listen`, adjust by appending only the missing bits. The base `valid_config()` must have neither, so the `(None, None)` path stays valid — confirm the existing `daemon_socket_path_defaults_and_parses` test still passes.)

- [ ] **Step 4: Run the config tests**

Run: `cargo test -p remanence-state config`
Expected: PASS (new + existing).

- [ ] **Step 5: Commit**

```bash
git add crates/remanence-state/src/config.rs
git commit -m "S2: daemon config listen + tls (mTLS) fields + validation"
```

---

## Task 2: Daemon `tls` module (cert loading)

**Files:**
- Modify: `crates/remanence-daemon/Cargo.toml`
- Create: `crates/remanence-daemon/src/tls.rs`
- Modify: `crates/remanence-daemon/src/lib.rs` (add `mod tls;` + re-export)

- [ ] **Step 1: Enable the `tls-ring` feature**

In `crates/remanence-daemon/Cargo.toml`, enable the TLS feature and add `thiserror` (used by the `tls` module's error type — it is a workspace dep but not yet listed for this crate):
```toml
tonic = { workspace = true, features = ["tls-ring"] }
thiserror = { workspace = true }
```

- [ ] **Step 2: Create the `tls` module**

Create `crates/remanence-daemon/src/tls.rs`:
```rust
//! Server-side mutual-TLS setup for the daemon's TCP listener (S2).
//!
//! Loads operator-provisioned PEM material (server identity + client CA) into a
//! tonic `ServerTlsConfig` that requires a client certificate. Cert generation,
//! rotation, and per-client authorization are out of scope.

use std::net::SocketAddr;

use remanence_state::DaemonTlsConfig;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

/// A resolved TCP/mTLS listener: where to bind and the TLS config to serve with.
pub struct TlsListener {
    pub addr: SocketAddr,
    pub tls: ServerTlsConfig,
}

/// Failure to load the daemon's TLS material.
#[derive(Debug, thiserror::Error)]
pub enum TlsConfigError {
    #[error("read TLS file {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Build a mutual-TLS `ServerTlsConfig` from operator PEM files: the server
/// identity (`cert` + `key`) and the CA that signs accepted client certs
/// (`client_ca`). Presence of `client_ca_root` makes a client certificate
/// mandatory.
pub fn load_server_tls(config: &DaemonTlsConfig) -> Result<ServerTlsConfig, TlsConfigError> {
    let read = |path: &std::path::Path| {
        std::fs::read(path).map_err(|source| TlsConfigError::Read {
            path: path.to_path_buf(),
            source,
        })
    };
    let cert = read(&config.cert)?;
    let key = read(&config.key)?;
    let client_ca = read(&config.client_ca)?;
    Ok(ServerTlsConfig::new()
        .identity(Identity::from_pem(cert, key))
        .client_ca_root(Certificate::from_pem(client_ca)))
}
```
(`thiserror` was added to `[dependencies]` in Step 1.)

- [ ] **Step 3: Wire the module**

In `crates/remanence-daemon/src/lib.rs`, add near the top (after the `use` lines):
```rust
mod tls;
pub use tls::{load_server_tls, TlsConfigError, TlsListener};
```

- [ ] **Step 4: Add cert-loading tests**

Append to `tls.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_server_tls_errors_on_missing_file() {
        let config = DaemonTlsConfig {
            cert: "/nonexistent/s.crt".into(),
            key: "/nonexistent/s.key".into(),
            client_ca: "/nonexistent/ca.crt".into(),
        };
        let err = load_server_tls(&config).expect_err("missing cert file");
        assert!(matches!(err, TlsConfigError::Read { .. }));
    }

    #[test]
    fn load_server_tls_builds_from_readable_pem_bytes() {
        // Identity/Certificate::from_pem store bytes (no parse at construction),
        // so readable files of any content build a ServerTlsConfig; real
        // validation happens at handshake (harness e2e).
        let dir = std::env::temp_dir().join(format!("rem-s2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = |name: &str| {
            let p = dir.join(name);
            std::fs::write(&p, b"-----BEGIN CERTIFICATE-----\nQQ==\n-----END CERTIFICATE-----\n").unwrap();
            p
        };
        let config = DaemonTlsConfig {
            cert: w("s.crt"),
            key: w("s.key"),
            client_ca: w("ca.crt"),
        };
        let _tls = load_server_tls(&config).expect("builds ServerTlsConfig");
    }
}
```

- [ ] **Step 5: Build + test**

Run: `cargo test -p remanence-daemon --lib tls`
Expected: PASS (2 tests). `cargo check -p remanence-daemon` clean (the `pub` items don't trip dead-code before Task 3 wires them).

- [ ] **Step 6: Commit**

```bash
git add crates/remanence-daemon/Cargo.toml crates/remanence-daemon/src/tls.rs crates/remanence-daemon/src/lib.rs
git commit -m "S2: daemon tls module — load mTLS ServerTlsConfig from PEM"
```

---

## Task 3: `serve()` dual transport + `main` wiring

**Files:**
- Modify: `crates/remanence-daemon/src/lib.rs`
- Modify: `crates/remanence-daemon/src/main.rs`
- Modify: `crates/remanence-daemon/tests/serve_catalog.rs`

- [ ] **Step 1: Rewrite `serve()` for both transports**

Replace the body of `serve()` in `lib.rs`. New signature adds `tls_listener: Option<TlsListener>`:
```rust
use std::sync::Arc;

/// Serve the Layer 5 services on the Unix socket and, when `tls_listener` is
/// `Some`, also on a TCP listener with mutual TLS — concurrently, until
/// `shutdown` resolves. The socket is unlinked on graceful shutdown.
pub async fn serve(
    state: ApiState,
    socket_path: &Path,
    tls_listener: Option<TlsListener>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let uds = UnixListener::bind(socket_path)?;

    // One shutdown source fans out to both servers.
    let notify = Arc::new(tokio::sync::Notify::new());
    let trigger = notify.clone();
    tokio::spawn(async move {
        shutdown.await;
        trigger.notify_waiters();
    });

    // Unix server.
    let unix_state = state.clone();
    let unix_notify = notify.clone();
    let unix_server = tokio::spawn(async move {
        Server::builder()
            .add_service(pb::daemon_server::DaemonServer::new(unix_state.daemon_service()))
            .add_service(pb::catalog_server::CatalogServer::new(unix_state.catalog_service()))
            .add_service(pb::write_session_service_server::WriteSessionServiceServer::new(
                unix_state.write_session_service(),
            ))
            .add_service(pb::read_session_service_server::ReadSessionServiceServer::new(
                unix_state.read_session_service(),
            ))
            .add_service(pb::library_service_server::LibraryServiceServer::new(
                unix_state.library_service(),
            ))
            .serve_with_incoming_shutdown(UnixListenerStream::new(uds), async move {
                unix_notify.notified().await
            })
            .await
    });

    // TCP + mTLS server (optional).
    let tcp_server = tls_listener.map(|listener| {
        let tcp_state = state.clone();
        let tcp_notify = notify.clone();
        tokio::spawn(async move {
            Server::builder()
                .tls_config(listener.tls)?
                .add_service(pb::daemon_server::DaemonServer::new(tcp_state.daemon_service()))
                .add_service(pb::catalog_server::CatalogServer::new(tcp_state.catalog_service()))
                .add_service(pb::write_session_service_server::WriteSessionServiceServer::new(
                    tcp_state.write_session_service(),
                ))
                .add_service(pb::read_session_service_server::ReadSessionServiceServer::new(
                    tcp_state.read_session_service(),
                ))
                .add_service(pb::library_service_server::LibraryServiceServer::new(
                    tcp_state.library_service(),
                ))
                .serve_with_shutdown(listener.addr, async move { tcp_notify.notified().await })
                .await
        })
    });

    let unix_result = unix_server.await?;
    if let Some(tcp_server) = tcp_server {
        tcp_server.await?.map_err(Box::new)?;
    }
    unix_result?;

    let _ = std::fs::remove_file(socket_path);
    Ok(())
}
```
Update the module doc comment (lines 1-3) to mention the TCP/mTLS transport. Note: the TCP task's closure returns `Result<(), tonic::transport::Error>` (both `tls_config(…)?` and `serve_with_shutdown(…).await` use that error); `map_err(Box::new)` lifts it into the boxed error. If the borrow checker objects to `?` inside the `.map(|listener| …)` closure returning a `JoinHandle`, keep the structure (the `?` is inside the inner `async move` block, which returns the `Result`).

- [ ] **Step 2: Update `main` to build the `TlsListener`**

In `main.rs`, after `state` is built and before the `serve(...)` call, add:
```rust
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
```
Change the serve call + log:
```rust
    if let Some(listener) = &tls_listener {
        eprintln!("rem-daemon: also serving mTLS on tcp:{}", listener.addr);
    }
    eprintln!(
        "rem-daemon: serving local Layer 5 API on unix:{}",
        socket_path.display()
    );
    match remanence_daemon::serve(state, &socket_path, tls_listener, shutdown_signal()).await {
```
(The `_ => None` arm is safe because `validate_config` already rejects the partial case; both-set is the only `Some` path.)

- [ ] **Step 3: Update the `serve_catalog` test call**

In `crates/remanence-daemon/tests/serve_catalog.rs`, the `serve(state, &socket_path, <shutdown>)` call gains a `None`:
```rust
    let server = tokio::spawn(async move {
        remanence_daemon::serve(state, &socket_path, None, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });
```
(Match the existing variable names — `state`, `socket_path`, the shutdown future — and insert `None` as the third argument.)

- [ ] **Step 4: Build + run the existing e2e**

Run: `cargo test -p remanence-daemon`
Expected: PASS — `serve_catalog` round-trips over the Unix socket with the new signature (`None` TLS), proving the dual-server refactor didn't regress the local transport.

- [ ] **Step 5: Commit**

```bash
git add crates/remanence-daemon/src/lib.rs crates/remanence-daemon/src/main.rs crates/remanence-daemon/tests/serve_catalog.rs
git commit -m "S2: serve unix + TCP/mTLS concurrently with shared shutdown"
```

---

## Task 4: read-only test + workspace gates

**Files:**
- Modify: `crates/remanence-daemon/tests/serve_catalog.rs` (or a new test file)

- [ ] **Step 1: Add a read-only-refuses-writes test**

Add a test that a `read_only`-built `ApiState` (no session owner — the shape `main` builds when `daemon.read_only`) refuses a state-changing write-session RPC. Use `get_write_session`: it only decodes the `session_id` then dispatches, so it reaches the `session_tx is None → unavailable` branch directly (unlike `open_write_session`, which validates the pool target first and would return `invalid_argument`).
```rust
#[tokio::test]
async fn read_only_state_refuses_write_session() {
    use remanence_api::pb::write_session_service_server::WriteSessionService as _;
    use remanence_api::{pb, ApiState};
    use remanence_state::CatalogIndex;
    use tonic::Request;

    let dir = std::env::temp_dir().join(format!("rem-s2-ro-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let index = CatalogIndex::open(dir.join("state.sqlite")).expect("open index");
    // No session owner == the read-only daemon shape (main uses new_with_config).
    let state = ApiState::new(index);
    let err = state
        .write_session_service()
        .get_write_session(Request::new(pb::GetWriteSessionRequest {
            session_id: vec![0u8; 16],
        }))
        .await
        .expect_err("read-only must refuse writes");
    assert_eq!(err.code(), tonic::Code::Unavailable);
}
```

- [ ] **Step 2: Format**

Run: `cargo fmt --all`; review `git diff --stat`.

- [ ] **Step 3: Clippy (workspace, `-D warnings`)**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 4: Test (workspace)**

Run: `cargo test --workspace`
Expected: PASS — config tests, tls tests, the read-only test, `serve_catalog`, and the full S1–S6b regression.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "S2: read-only-refuses-writes test + fmt"
```

---

## Self-Review (completed during planning)

**Spec coverage:** `tls-ring` (Task 2) ✓; `listen` + `tls` config + validation (Task 1) ✓; cert loading → `ServerTlsConfig` with client cert required (Task 2) ✓; `serve()` unix + TCP/mTLS concurrent with shared `Notify` shutdown (Task 3) ✓; `main` wiring + addr parse + cert-load error exits (Task 3) ✓; read-only-refuses-writes (Task 4) ✓; the all-or-nothing rule enforced in config (Task 1) and relied on in `main` (Task 3) ✓. AC4 (real mTLS handshake + ring-provider confirmation) is the human-run harness e2e.

**Placeholder scan:** none — every step has concrete code/commands. The three previously-open items are resolved against the real source: `require_absolute` uses `StateError::ConfigInvalid(String)`; `thiserror` is added to the daemon crate in Task 2 Step 1; the read-only test uses `get_write_session` (which reaches the no-owner `unavailable` branch from a bare `session_id`).

**Type consistency:** `TlsListener { addr: SocketAddr, tls: ServerTlsConfig }` is built in `main` and consumed in `serve()`; `serve()`'s new third parameter is `Option<TlsListener>` at every call site (`main` + `serve_catalog` test → `None`); `load_server_tls(&DaemonTlsConfig) -> Result<ServerTlsConfig, TlsConfigError>` matches the `main` call; `tls_config` is invoked on `Server` before `add_service` (verified ordering).
