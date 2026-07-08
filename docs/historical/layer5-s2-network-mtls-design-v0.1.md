# Layer 5 S2 — Network transport hardening (mTLS over TCP) Design v0.1

Status: design decision. S1/S4a/S5a/S3a/S6a/S6b are live; the full Layer 5
surface (catalog, data plane, operations, robotics) is built but reachable
**only over a local Unix socket with no authentication**. S2 adds a **TCP
listener with mutual TLS** so a remote client (sutradhara on a different host) can
reach the daemon securely, served **concurrently** with the existing Unix socket,
and confirms `read_only` enforcement. Grounds in `remanence-daemon` (`serve` +
`main`), `remanence-state::DaemonConfig`, and tonic 0.14.5's rustls-based TLS
(`tonic::transport::{ServerTlsConfig, Identity, Certificate}`, gated by the
`tls-ring` feature).

## Background — what exists

- **`serve()`** binds a single `UnixListener` and serves the five services
  (Daemon, Catalog, WriteSession, ReadSession, Library) via
  `Server::builder().add_service(…)·…·serve_with_incoming_shutdown(uds, shutdown)`;
  it removes a stale socket before bind and unlinks it on shutdown. No TLS, no
  auth — anyone with filesystem access to the socket has full access.
- **`DaemonConfig`** (`#[serde(deny_unknown_fields)]`): `state_dir`,
  `default_idle_timeout_seconds`, `read_only` ("state-changing operations must be
  rejected"), `socket_path: Option`. Config validation requires absolute
  `state_dir`/`socket_path`.
- **`read_only`** is enforced *implicitly*: `main` builds `ApiState::new_with_config`
  (no session owner) instead of `with_session_owner`, so every write/read-session/
  robotics RPC returns `unavailable` ("daemon has no session owner (read-only
  mode)"). Catalog + Library-read RPCs serve normally.
- **tonic 0.14.5** has no TLS feature enabled. `tls-ring` unlocks `_tls-any` →
  `ServerTlsConfig::new().identity(Identity::from_pem(cert,key)).client_ca_root(Certificate::from_pem(ca))`
  + `Server::tls_config(cfg) -> Result<Self, Error>` (verified 2026-06-04).

## Architecture

**1. Dependency.** Enable `tls-ring` on the daemon's `tonic` (pulls
`tokio-rustls` + `rustls` + `ring`).

**2. Config** (`DaemonConfig`, new explicit fields):
- `listen: Option<String>` — TCP bind address, e.g. `"0.0.0.0:8443"`.
- `tls: Option<DaemonTlsConfig>` — new struct `{ cert: PathBuf, key: PathBuf, client_ca: PathBuf }`:
  the server's identity cert + private key, and the CA whose signature a client
  cert must carry.

Validation (in `validate_config`): `listen` and `tls` are **all-or-nothing**
(exactly one present → error: "daemon.listen and daemon.tls must be set
together"); when present, the three `tls` paths must be absolute (like
`state_dir`), and `listen` must parse as a `SocketAddr`. The Unix socket is
unaffected and always served.

**3. Cert loading** — a daemon-crate `tls` module: `load_server_tls(&DaemonTlsConfig)
-> Result<ServerTlsConfig, TlsConfigError>` reads the three PEM files and builds
`ServerTlsConfig::new().identity(Identity::from_pem(cert, key)).client_ca_root(Certificate::from_pem(client_ca))`.
Presence of `client_ca_root` (with `client_auth_optional` left default-`false`)
makes a **valid client certificate mandatory** — true mutual TLS. A missing /
unreadable / unparseable PEM yields a clear `TlsConfigError`; `main` prints it and
exits non-zero (mirroring the existing config-load failure path).

**4. `serve()` — both transports concurrently.** Signature gains an
`Option<TlsListener>` (`struct TlsListener { addr: SocketAddr, tls: ServerTlsConfig }`,
built by `main` from config). Behavior:
- Always bind + serve the Unix socket (unchanged registration of all five
  services).
- When `Some(tls_listener)`, also bind a `TcpListener` and serve it with
  `Server::builder().tls_config(tls)?·…services…·serve_with_shutdown(addr, signal)`.
- **Shared shutdown:** a small task awaits the passed-in `shutdown` future and
  fires a `tokio::sync::Notify`; each server's shutdown signal is
  `notify.notified()`. `serve()` `tokio::spawn`s both servers and `tokio::join!`s
  them, returning the first error (or `Ok` when both stop). The socket file is
  unlinked on exit. `ApiState` is `Clone`, so both servers share one backend
  (one session owner, one catalog) — the single-session and operation semantics
  are unchanged; the transport is just two front doors.

The five `.add_service(…)` registrations are written inline in each server block
(verified shape) rather than factored — `tls_config` lives on `Server` (pre-`add_service`)
and a `Router` can serve only once, so the two servers are built separately.

**5. `read_only`.** Kept as-is — a `read_only` daemon has no session owner, so
mutating RPCs already fail; a `read_only` + mTLS daemon is exactly the intended
**remote read-replica** (serves Catalog + Library-read to authenticated clients,
refuses writes). S2 adds a test asserting a `read_only` daemon refuses a write,
and documents the shape. No new enforcement code.

## Trust model (operator-provisioned)

mTLS here proves "the peer holds a certificate signed by the configured CA" — a
*trusted client*, not *which* client. The operator provisions the server
cert/key and the client CA (and issues client certs) **out of band**. Per-client
identity → authorization (RBAC), cert generation/rotation, and CRL/OCSP are
**out of scope** (future work). Standard rustls verification applies (validity,
signature chain to `client_ca`).

## Error handling

- Partial/!absolute/unparseable TLS config → `validate_config` error → `main`
  exits non-zero before serving.
- Cert file missing/unreadable/unparseable → `TlsConfigError` → `main` exits
  non-zero.
- TCP bind failure (port in use, perms) → `serve()` returns the bind error.
- A TLS handshake failure (no/expired/untrusted client cert) is handled by
  tonic/rustls per-connection — the connection is refused; other connections and
  the Unix socket are unaffected.

## Known runtime check (for the harness e2e)

rustls 0.23 requires a process crypto provider. The `tls-ring` feature wires
`ring` as tonic's provider, so no explicit `CryptoProvider::install_default()`
should be needed — but this only manifests at the first real TLS handshake, so
the harness e2e must confirm a real mTLS connect succeeds (not just that it
compiles/binds).

## Pinned contract for the operator / consumer

- Config: `[daemon] listen = "host:port"` + `[daemon.tls] cert/key/client_ca`
  (all absolute) enables mTLS over TCP, served alongside the Unix socket.
- A client must present a cert signed by `client_ca` to use the TCP endpoint;
  on-host tools may keep using the Unix socket without a cert.
- `read_only = true` + mTLS = a remote read replica: Catalog/Library-read work;
  writes/robotics return `unavailable`.

## Scope

**IN (S2):** the `tls-ring` feature; `DaemonConfig.listen` + `DaemonConfig.tls`
(`DaemonTlsConfig`) + their validation; the `tls` cert-loading module;
`serve()` running the Unix socket + a TCP/mTLS listener concurrently with a
shared `Notify` shutdown; `main` wiring (load certs, parse addr, build
`TlsListener`); a `read_only`-refuses-writes test.

**OUT:** cert generation / rotation / management (operator-provided PEMs);
per-client identity → authorization / RBAC; CRL/OCSP revocation; hostname/SAN
pinning policy beyond standard chain verification; a CLI `--listen`/`--tls`
override (config-only); an in-process mTLS round-trip test (the handshake is
validated in the harness e2e; an `rcgen`-based test is a possible later add);
HTTP/health-only ports; the manual hardware/remote harness e2e.

## Acceptance criteria

1. **Unit (config):** `listen`+`tls` both present parses; exactly one present →
   validation error; a relative `tls.cert`/`key`/`client_ca` → error; an
   unparseable `listen` → error; neither present → valid (unix-only, unchanged).
2. **Unit (cert loading):** `load_server_tls` returns a clear `TlsConfigError`
   for a missing/unreadable PEM path; given readable PEM bytes it builds a
   `ServerTlsConfig` (smoke — construct from in-test bytes, no network).
3. **Integration (hardware-free):** the existing `serve_catalog` Unix-socket e2e
   stays green with the new `serve()` signature (`None` TLS) — proving the
   dual-server refactor didn't regress the local transport; a `read_only`
   `ApiState` returns `unavailable` from a write-session open.
4. **Harness e2e (human-run, OUT of Codex scope):** with `[daemon] listen` +
   `[daemon.tls]` set on the akash host, a remote tonic client **with** a
   client cert signed by `client_ca` round-trips `Daemon.Health` + `Catalog`
   over mTLS; a client with **no** / an **untrusted** cert is refused; the local
   Unix socket still works; a real TLS connect confirms the `ring` crypto
   provider is wired (the known-runtime check above).
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-daemon` + `cargo clippy -p
remanence-daemon --all-targets -- -D warnings` (both clean) on 2026-06-04 with a
skeleton (`crates/remanence-daemon/src/s2_skeleton.rs`, since removed —
design-only) plus the transient `tls-ring` feature: `ServerTlsConfig::new()
.identity(Identity::from_pem(…)).client_ca_root(Certificate::from_pem(…))` built,
and the **two concurrent servers** — unix via `serve_with_incoming_shutdown` and
TCP via `Server::builder().tls_config(tls)?·…·serve_with_shutdown(addr, signal)`
— sharing a `tokio::sync::Notify`, spawned and `tokio::join!`-ed, type-checked.

Five-category result:

1. **Module privacy** — new `tls` module + `serve()` changes live in
   `remanence-daemon`; the config structs in `remanence-state` are `pub`. Pass.
2. **`!Send` in threading / async** — `ApiState` is `Clone + Send + 'static`
   (moves into both `tokio::spawn`ed servers); `ServerTlsConfig`/the `Notify` are
   `Send + Sync`; the two server futures are `Send`. Pass.
3. **Reactor-registration timing** — both `UnixListener::bind`/`TcpListener` and
   the tonic servers are created **inside** the async `serve()` (already on the
   tokio runtime); no construction-before-runtime. Pass.
4. **Borrowed-handle plumbing** — no borrows held across the spawns; `ApiState`
   is cloned per server. Pass.
5. **Trait/method visibility** — `tonic::transport::{Server, ServerTlsConfig,
   Identity, Certificate}` reachable with `tls-ring`; `Server::tls_config`,
   `Router::{serve_with_incoming_shutdown, serve_with_shutdown}` resolve. Pass.

New dependency: `tls-ring` on `tonic` (pulls `tokio-rustls`/`rustls`/`ring`); no
new direct crates.
