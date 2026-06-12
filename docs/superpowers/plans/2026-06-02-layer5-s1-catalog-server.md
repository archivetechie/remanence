# Layer 5 S1 — Catalog Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stand up a runnable `rem-daemon` that serves the existing read-only `Daemon` + `Catalog` gRPC services over a Unix-domain dev socket, reachable by the `rem` CLI client.

**Architecture:** A new `remanence-daemon` crate with a `serve(state, socket, shutdown)` library fn (tonic `Server` over a `UnixListener`, registering `DaemonServer` + `CatalogServer`) and a thin `rem-daemon` bin (load config → open index → `ApiState` → serve with SIGINT/SIGTERM shutdown). A shared `connect_unix` connector in `remanence-api` lets the CLI client (and the daemon's own test) dial the socket. Read-only: services open `CatalogIndex::open_read_only` per request, so no writer-ownership.

**Tech Stack:** Rust, `tonic 0.14` (Unix-socket transport), `tokio`, `tokio-stream`, `tower` + `hyper-util` (UDS connector), `clap`; crates `remanence-daemon` (new), `remanence-api`, `remanence-state`, `remanence-cli`.

Spec: `docs/layer5-catalog-server-design-v0.1.md`. `serve()` + `connect_unix()` were `cargo check` + clippy verified on 2026-06-02 (skeleton since removed); this plan recreates that code.

**Gates (run before every commit):**
```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p <crate touched>
```
Full-workspace gates in the final task.

---

## File Structure

- `crates/remanence-state/src/config.rs` — add `socket_path` to `DaemonConfig` (Task 1).
- `crates/remanence-api/src/lib.rs` + `Cargo.toml` — add `connect_unix` + `tower`/`hyper-util` deps (Task 2).
- `crates/remanence-daemon/` (new) — `Cargo.toml`, `src/lib.rs` (`serve`), `src/main.rs` (`rem-daemon` bin), integration test (Tasks 3, 4).
- `Cargo.toml` (workspace) — add the new member + `tower`/`hyper-util` workspace deps (Tasks 2, 3).
- `crates/remanence-cli/src/lib.rs` — `connect_daemon` `unix:` branch (Task 5).

---

### Task 1: Add `socket_path` to `DaemonConfig`

**Files:**
- Modify: `crates/remanence-state/src/config.rs` (`DaemonConfig` at :39; validation near :215)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `config.rs`:

```rust
#[test]
fn daemon_socket_path_defaults_and_parses() {
    // Absent → default to <state_dir>/rem.sock
    let config = parse_config_toml(&valid_config()).expect("valid config");
    assert_eq!(config.daemon.socket_path, None);
    assert_eq!(
        config.daemon.socket_path_or_default(),
        config.daemon.state_dir.join("rem.sock")
    );

    // Present + absolute → used as-is.
    let text = valid_config().replace(
        "default_idle_timeout_seconds = 1800",
        "default_idle_timeout_seconds = 1800\nsocket_path = \"/run/rem/rem.sock\"",
    );
    let config = parse_config_toml(&text).expect("valid config with socket_path");
    assert_eq!(
        config.daemon.socket_path_or_default(),
        std::path::PathBuf::from("/run/rem/rem.sock")
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-state daemon_socket_path_defaults_and_parses`
Expected: FAIL to compile — `socket_path` field / `socket_path_or_default` don't exist.

- [ ] **Step 3: Add the field + helper + validation**

In `struct DaemonConfig` (`config.rs:39`), add after `read_only`:

```rust
    /// Unix-domain socket the daemon listens on (dev transport). Absent →
    /// `<state_dir>/rem.sock`.
    #[serde(default)]
    pub socket_path: Option<PathBuf>,
```

Add an impl (next to the struct):

```rust
impl DaemonConfig {
    /// Resolve the listen socket: explicit `socket_path`, else `<state_dir>/rem.sock`.
    pub fn socket_path_or_default(&self) -> PathBuf {
        self.socket_path
            .clone()
            .unwrap_or_else(|| self.state_dir.join("rem.sock"))
    }
}
```

In the validation fn (near `config.rs:215`, alongside `require_absolute("daemon.state_dir", ...)`), add:

```rust
    if let Some(socket_path) = &config.daemon.socket_path {
        require_absolute("daemon.socket_path", socket_path)?;
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p remanence-state daemon_socket_path_defaults_and_parses`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-state --all-targets -- -D warnings
git add crates/remanence-state/src/config.rs
git commit -m "config: add [daemon] socket_path (Layer 5 S1)"
```

---

### Task 2: Add `connect_unix` to `remanence-api`

**Files:**
- Modify: `Cargo.toml` (workspace deps), `crates/remanence-api/Cargo.toml`, `crates/remanence-api/src/lib.rs`

- [ ] **Step 1: Add `tower` + `hyper-util` workspace deps**

In the root `Cargo.toml` `[workspace.dependencies]` table, add (versions match the existing lock — tower 0.5.3, hyper-util 0.1.20):

```toml
tower               = "0.5"
hyper-util          = "0.1"
```

In `crates/remanence-api/Cargo.toml` `[dependencies]`, add:

```toml
tower = { workspace = true }
hyper-util = { workspace = true }
```

- [ ] **Step 2: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `remanence-api/src/lib.rs`:

```rust
#[tokio::test]
async fn connect_unix_to_missing_socket_fails() {
    let dir = tempfile::tempdir().unwrap_or_else(|_| panic!("tempdir"));
    let missing = dir.path().join("nope.sock");
    // Channel connection is lazy in tonic; force a real connection attempt.
    let result = crate::connect_unix(missing).await;
    assert!(result.is_err(), "connecting to a missing socket must error");
}
```

(If `remanence-api` lacks a `tempfile`/`tempdir` dev-dep, use the crate's existing temp-dir helper — search the test module for how other tests make a temp dir — instead of `tempfile`.)

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p remanence-api connect_unix_to_missing_socket_fails`
Expected: FAIL to compile — `connect_unix` doesn't exist.

- [ ] **Step 4: Implement `connect_unix`**

Add to `remanence-api/src/lib.rs` (top-level, after the `pub mod pb { … }` block). This is the verified connector:

```rust
use std::path::PathBuf;
use tonic::transport::{Channel, Endpoint, Uri};

/// Connect a gRPC channel to a Unix-socket daemon (Layer 5 dev transport).
/// The URI authority is a placeholder ignored by the custom connector.
pub async fn connect_unix(socket_path: PathBuf) -> Result<Channel, tonic::transport::Error> {
    Endpoint::try_from("http://[::1]:50051")?
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = socket_path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p remanence-api connect_unix_to_missing_socket_fails`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-api --all-targets -- -D warnings
git add Cargo.toml crates/remanence-api/Cargo.toml crates/remanence-api/src/lib.rs
git commit -m "api: add connect_unix UDS client connector (Layer 5 S1)"
```

---

### Task 3: Create the `remanence-daemon` crate (serve + bin)

**Files:**
- Modify: `Cargo.toml` (workspace member)
- Create: `crates/remanence-daemon/Cargo.toml`, `crates/remanence-daemon/src/lib.rs`, `crates/remanence-daemon/src/main.rs`

- [ ] **Step 1: Add the workspace member**

In root `Cargo.toml` `[workspace] members`, add after `"crates/remanence-cli",`:

```toml
    "crates/remanence-daemon",
```

- [ ] **Step 2: Create `crates/remanence-daemon/Cargo.toml`**

```toml
[package]
name = "remanence-daemon"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
authors.workspace = true
license.workspace = true
repository.workspace = true

[[bin]]
name = "rem-daemon"
path = "src/main.rs"

[dependencies]
remanence-api = { path = "../remanence-api" }
remanence-state = { path = "../remanence-state" }
clap = { workspace = true }
tokio = { workspace = true }
tokio-stream = { workspace = true, features = ["net"] }
tonic = { workspace = true }

[dev-dependencies]
tempdir = "0.3"
```

- [ ] **Step 3: Create `crates/remanence-daemon/src/lib.rs` with `serve()`**

```rust
//! Layer 5 read-only catalog server. Serves the Daemon + Catalog gRPC
//! services over a Unix-domain socket (dev transport). mTLS/TCP is S2.

use std::future::Future;
use std::path::Path;

use remanence_api::{pb, ApiState};
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;

/// Serve the Daemon + Catalog services on `socket_path` until `shutdown`
/// resolves. A stale socket from a prior unclean exit is removed before bind;
/// the socket file is unlinked on graceful shutdown.
pub async fn serve(
    state: ApiState,
    socket_path: &Path,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;

    Server::builder()
        .add_service(pb::daemon_server::DaemonServer::new(state.daemon_service()))
        .add_service(pb::catalog_server::CatalogServer::new(state.catalog_service()))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), shutdown)
        .await?;

    let _ = std::fs::remove_file(socket_path);
    Ok(())
}
```

- [ ] **Step 4: Create `crates/remanence-daemon/src/main.rs` (the `rem-daemon` bin)**

```rust
//! rem-daemon — Layer 5 read-only catalog server entrypoint.

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
            eprintln!("error: open index {}: {error}", config.index.sqlite_path.display());
            return ExitCode::from(1);
        }
    };
    let state = remanence_api::ApiState::new_with_config(index, &config);

    eprintln!("rem-daemon: serving catalog on unix:{}", socket_path.display());
    match remanence_daemon::serve(state, &socket_path, shutdown_signal()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: serve: {error}");
            ExitCode::from(1)
        }
    }
}
```

- [ ] **Step 5: Build to verify the crate compiles**

Run: `cargo build -p remanence-daemon`
Expected: compiles (both lib + `rem-daemon` bin). `CatalogIndex::open`, `load_config`, `ApiState::new_with_config`, `config.daemon.socket_path_or_default` (Task 1), and the tonic UDS server chain all resolve.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-daemon --all-targets -- -D warnings
git add Cargo.toml crates/remanence-daemon/
git commit -m "Add remanence-daemon crate: serve read-only catalog over UDS (Layer 5 S1)"
```

---

### Task 4: Integration test — serve + client round-trip over a temp socket

**Files:**
- Create: `crates/remanence-daemon/tests/serve_catalog.rs`

- [ ] **Step 1: Write the failing test**

Create `crates/remanence-daemon/tests/serve_catalog.rs`:

```rust
//! End-to-end: serve() over a temp Unix socket, then a real gRPC client
//! round-trips Daemon.health and Catalog.list_tape_pools through it.

use remanence_api::{pb, ApiState};
use remanence_state::index::TapePoolProjectionInput;
use remanence_state::CatalogIndex;

#[tokio::test]
async fn serve_catalog_roundtrips_health_and_pools_over_unix_socket() {
    let dir = tempdir::TempDir::new("rem-daemon-it").expect("tempdir");
    let socket_path = dir.path().join("rem.sock");

    // Seed a catalog with one pool.
    let mut index = CatalogIndex::open(dir.path().join("state.sqlite")).expect("open index");
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: "scenario-a".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("seed pool");
    let state = ApiState::new(index);

    // Serve in the background; shut down via a oneshot.
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_socket = socket_path.clone();
    let server = tokio::spawn(async move {
        remanence_daemon::serve(state, &serve_socket, async {
            let _ = shutdown_rx.await;
        })
        .await
        .expect("serve");
    });

    // Wait for the socket to appear (serve binds asynchronously).
    for _ in 0..100 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(socket_path.exists(), "daemon did not create the socket");

    let channel = remanence_api::connect_unix(socket_path.clone())
        .await
        .expect("connect unix");

    // Daemon.health
    let mut daemon = pb::daemon_client::DaemonClient::new(channel.clone());
    daemon.health(()).await.expect("health");

    // Catalog.list_tape_pools returns the seeded pool.
    let mut catalog = pb::catalog_client::CatalogClient::new(channel);
    let pools = catalog
        .list_tape_pools(pb::ListTapePoolsRequest::default())
        .await
        .expect("list_tape_pools")
        .into_inner()
        .pools;
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].pool_id, "scenario-a");

    let _ = shutdown_tx.send(());
    server.await.expect("server task");
    assert!(!socket_path.exists(), "socket should be unlinked on shutdown");
}
```

(Confirm the exact `TapePoolProjectionInput` field set against `remanence-state/src/index.rs:86` and `ListTapePoolsRequest` against `pb`; adjust field names if the structs differ. If `remanence_state::index` is private, use the re-exported path — `grep "pub use index" crates/remanence-state/src/lib.rs`.)

- [ ] **Step 2: Run test to verify it fails, then passes**

Run: `cargo test -p remanence-daemon --test serve_catalog`
Expected first run: FAIL only if a name needs adjusting (fix per the note). Once names match: PASS — proves the server binds the socket, serves both services, the client connects over UDS, the seeded pool round-trips, and the socket is unlinked on shutdown.

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-daemon --all-targets -- -D warnings
git add crates/remanence-daemon/tests/serve_catalog.rs
git commit -m "Integration test: rem-daemon serves catalog over UDS (Layer 5 S1)"
```

---

### Task 5: CLI — connect over `unix:` endpoints

**Files:**
- Modify: `crates/remanence-cli/src/lib.rs` (`connect_daemon`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `remanence-cli/src/lib.rs`:

```rust
#[tokio::test]
async fn connect_daemon_unix_scheme_routes_to_unix_connector() {
    // A unix: endpoint to a missing socket must fail at connect (not at parse),
    // proving it took the UDS path rather than the http parser.
    let dir = tempdir::TempDir::new("rem-cli-unix").expect("tempdir");
    let missing = dir.path().join("nope.sock");
    let endpoint = format!("unix:{}", missing.display());
    let result = connect_daemon(&endpoint).await;
    assert!(result.is_err(), "missing unix socket must error at connect");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-cli connect_daemon_unix_scheme_routes_to_unix_connector`
Expected: FAIL — `connect_daemon` feeds `unix:…` to `Channel::from_shared`, which errors at parse with a different message, or the test reaches code that doesn't compile if the branch is absent. (If it happens to error for the wrong reason, Step 3 makes it error for the right one.)

- [ ] **Step 3: Add the `unix:` branch to `connect_daemon`**

Replace `connect_daemon` in `remanence-cli/src/lib.rs` with:

```rust
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
```

(`remanence_api` is already a dependency of `remanence-cli`; `connect_unix` is `pub` from Task 2.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p remanence-cli connect_daemon_unix_scheme_routes_to_unix_connector`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-cli --all-targets -- -D warnings
git add crates/remanence-cli/src/lib.rs
git commit -m "cli: connect_daemon supports unix: socket endpoints (Layer 5 S1)"
```

---

### Task 6: Full verification + manual smoke

- [ ] **Step 1: Full-workspace gates**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: all pass (10 crates now, including `remanence-daemon`).

- [ ] **Step 2: Manual smoke (local, no hardware)**

```bash
cargo build --release -p remanence-daemon -p remanence-cli
# Against an existing dev state (e.g. /var/lib/replica/rem/config.toml):
target/release/rem-daemon --config /var/lib/replica/rem/config.toml &
SOCK=/var/lib/replica/rem/rem.sock   # or the configured socket_path
target/release/rem catalog --endpoint "unix:$SOCK" pools
target/release/rem catalog --endpoint "unix:$SOCK" tapes
kill %1   # SIGTERM → graceful shutdown, socket unlinked
```
Expected: `pools`/`tapes` return the catalog contents over the socket; on `kill`, the daemon shuts down cleanly and removes the socket file.

- [ ] **Step 3: Record the result in the journal**

Append a dated entry noting the S1 smoke result (or that it is pending).

---

## Self-Review

**Spec coverage** (against `docs/layer5-catalog-server-design-v0.1.md`):
- `remanence-daemon` crate + `serve` + `rem-daemon` bin → Task 3. ✓
- UDS dev transport (bind, stale-cleanup, graceful shutdown, unlink) → Task 3 Step 3 + Task 4 assertions. ✓
- Daemon + Catalog registered; other services not → Task 3 Step 3. ✓
- CLI `unix:` connector → Tasks 2 (connect_unix) + 5 (connect_daemon branch). ✓
- `socket_path` config → Task 1. ✓
- Read-only / per-request opens / RW-once-at-startup → Task 3 (`CatalogIndex::open` in the bin; services open read-only per request as they already do). ✓
- Integration test over temp socket → Task 4. ✓
- OUT (mTLS, sessions, library, audit, ops/reconcile, list_files_in_object/get_file) → not in any task, by design. ✓

**Placeholder scan:** none. Every code step shows full code; commands have expected output. Two "confirm the exact field/path names" notes (Task 4's `TapePoolProjectionInput`/`ListTapePoolsRequest`, the `remanence_state::index` re-export) are concrete grep-then-adjust instructions, not deferred work.

**Type consistency:** `serve(state: ApiState, socket_path: &Path, shutdown: impl Future<Output=()> + Send + 'static)` is identical in Task 3 (def), Task 4 (call), and the bin (Task 3 Step 4 call). `connect_unix(PathBuf) -> Result<Channel, tonic::transport::Error>` matches across Task 2 (def), Task 4 (use), Task 5 (use). `config.daemon.socket_path_or_default()` defined in Task 1, used in the bin (Task 3 Step 4). `ApiState::new_with_config(index, &config)` (bin) and `ApiState::new(index)` (test) are both real constructors.

**Verified against code:** `serve()` + `connect_unix()` are the 2026-06-02 cargo-check + clippy-clean skeleton code (tonic 0.14 UDS, `Send+Sync+'static` service bounds hold). `ApiState::{new, new_with_config, daemon_service, catalog_service}`, `remanence_api::pb::{daemon_server::DaemonServer, catalog_server::CatalogServer, daemon_client, catalog_client}`, `load_config`, `config.index.sqlite_path`, `DaemonConfig`, `upsert_tape_pool_projection` all confirmed at current HEAD. `tower 0.5.3` + `hyper-util 0.1.20` already in `Cargo.lock` (transitive).
