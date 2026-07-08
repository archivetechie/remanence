# Layer 5 gRPC — Catalog Server (S1) Design v0.1

Status: design decision + Layer 5 scoping record. Defines the Layer 5 slice
decomposition (S1–S7) and designs **S1: the read-only catalog server** — a
runnable `rem-daemon` that serves the existing `Daemon` + `Catalog` read RPCs
over a Unix-domain socket, reachable by the existing `rem` CLI client. Relates
to `docs/spec-v0.4.md` §11 (Layer 5) and `proto/layer5.proto`.

## Background — Layer 5 today

`proto/layer5.proto` defines six services. `remanence-api` implements four
server-side (`Daemon`, `Catalog`, `WriteSessionService`, `ReadSessionService`),
index-path-backed (`CatalogIndex::open_read_only` per request). The Catalog read
surface is functionally complete: the only `unimplemented` arms are
deliberately-deferred *variants* (library-scoped `list_tapes`, reconcile-from-tape
`enumerate_objects`, refresh-from-source `enumerate_units`) plus two genuinely
unbuilt RPCs (`list_files_in_object`, `get_file`).

**The load-bearing gap:** there is **no server entrypoint** — no daemon binary,
no `serve()`, no transport, no socket bind anywhere outside an in-process test
(`remanence-cli/src/lib.rs`, `rem_daemon_health_roundtrips...`). The service impls
are real but unreachable; the CLI client targets `http://127.0.0.1:8443` and
nothing listens. So "catalog server" is about *making the catalog service serve*.

## Layer 5 slice decomposition (scoping)

Too large for one spec; sliced by dependency:

- **S1 — Catalog server (read-only), Unix-socket dev transport** ← *this doc*.
- **S2 — Transport hardening**: mTLS over TCP:8443, cert/config, `read_only` enforcement.
- **S3 — Operations + cancellation**: `list_operations`/`cancel`/`watch`, per-op ring buffer, the `OperationRef` path (`reconcile_tape`).
- **S4 — WriteSessionService server**: single-writer owner task.
- **S5 — ReadSessionService server**: production daemon read (`ReadObjectRange`/`ReadFile`); the rem-debug `archive read`/`verify` are the break-glass counterpart.
- **S6 — LibraryService server**: list/inspect + move/load/unload/import/export + `StreamLibraryEvents` (the hot-plug watcher consumer).
- **S7 — Audit server**: `QueryAudit`.

Each is its own design→plan→implement cycle. S1 is the anchor: it turns today's
dead-code catalog impl into something Sutradhara/the CLI can hit, and is
self-contained (read-only, no hardware, no sessions).

## S1 architecture

A new **`remanence-daemon`** crate produces a `rem-daemon` binary and a `serve()`
library function. It depends on `remanence-api` (the service impls + `pb`) and
`remanence-state` (config/index).

```rust
// crates/remanence-daemon/src/lib.rs — verified, see §verification
pub async fn serve(
    index_path: &Path,
    socket_path: &Path,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let index = CatalogIndex::open(index_path)?;          // RW once → apply migrations
    let state = ApiState::new(index);                     // (new_with_config in impl, for pools)
    if socket_path.exists() { std::fs::remove_file(socket_path)?; }   // clear stale socket
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

The `rem-daemon` bin is thin: parse `--config` (+ optional `--socket` override),
build a tokio runtime, load `RemConfig`, resolve the index + socket paths, install
a SIGINT/SIGTERM handler as the `shutdown` future, and `block_on(serve(...))`.

This mirrors the existing in-process test server (TCP → UDS, plus the second
service and graceful shutdown).

## Client (CLI) — connect over the Unix socket

`remanence-cli::connect_daemon` learns to dial a Unix socket. Verified connector
(needs new deps `tower` + `hyper-util` on `remanence-cli`):

```rust
pub async fn connect_unix(socket_path: PathBuf) -> Result<Channel, tonic::transport::Error> {
    Endpoint::try_from("http://[::1]:50051")?   // authority ignored by the connector
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = socket_path.clone();
            async move {
                let stream = UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
            }
        }))
        .await
}
```

The daemon-client commands accept a `unix:<path>` endpoint (or default to the
config socket path). The existing `http://` path stays for S2's mTLS.

## Config

Add `socket_path` to the `[daemon]` section of `RemConfig` (default
`<state_dir>/rem.sock`). The bin uses it unless `--socket` overrides.

## What's served

Register **`Daemon` + `Catalog`** only. Within those, deferred RPCs return
`Status::unimplemented` honestly (the server is up; clients get a clear code):
`list_operations`(filter)/`cancel_operation`/`watch_operation` (S3),
`reconcile_tape`/reconcile-from-tape/refresh-from-source variants (S3/S6),
library-scoped `list_tapes` (S6). `WriteSession`/`ReadSession`/`Library`/`Audit`
services are **not registered** in S1.

The read RPCs the CLI client exercises (`list_tapes` pool filter, `get_tape`,
`list_tape_files`, `list_tape_pools`, `get_tape_pool`, `enumerate_units`,
`get_catalog_unit`, `list_entries_in_unit`) already work and become reachable.

## State / concurrency

Read-only catalog: each request opens `CatalogIndex::open_read_only(index_path)`
(existing `CatalogService` behaviour). The daemon opens the index **read-write
once at startup** to apply migrations, then serves read-only queries. No
single-writer owner task (that arrives with S4/S5). Single process; UDS perms
`0600`; no network listener, no TLS.

## Error handling / shutdown

Startup errors (index open, socket bind) abort with a clear message + non-zero
exit. SIGINT/SIGTERM → the `shutdown` future fires → `serve_with_incoming_shutdown`
drains in-flight requests → the socket file is unlinked. A stale socket from a
prior unclean exit is removed before bind.

## Scope

**IN:** the `remanence-daemon` crate (`serve` + `rem-daemon` bin), UDS dev
transport (bind/perms/stale-cleanup/graceful shutdown), the CLI `unix:` connector
(+ `tower`/`hyper-util` deps), `socket_path` config, registering `Daemon` +
`Catalog`, and an integration test serving over a temp socket.

**OUT:** mTLS (S2); operations/cancel/watch + `reconcile_tape` (S3); write/read
sessions (S4/S5); library + `StreamLibraryEvents` (S6); audit (S7);
`list_files_in_object`/`get_file` (need new Layer-4 queries, not CLI-exercised —
a small catalog fast-follow).

## Testing

- **Integration (no hardware):** a test that `serve()`s on a temp Unix socket (a
  `TempDir` socket path) against a populated temp `CatalogIndex`, then connects a
  client via `connect_unix` and round-trips `Daemon.health` + a `Catalog` read
  (`list_tape_pools`). Mirrors the existing in-process TCP test, UDS instead.
- **CLI e2e:** `rem catalog --endpoint unix:<path> pools` against a running
  `rem-daemon` returns the seeded pools.
- **Manual:** run `rem-daemon --config …` against the akash state; `rem catalog`
  commands over the default socket.
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-daemon` + `cargo clippy -p
remanence-daemon --all-targets -- -D warnings` (both clean) on 2026-06-02, with a
real-body skeleton crate (`serve()` + `connect_unix()` as shown above, a thin
`main.rs`, added to the workspace). The skeleton was **removed after
verification** (design-only); the plan recreates it. Compiled against current
HEAD (post A.9/B.7/C.7).

Five-category result:
1. **Module privacy** — the new crate consumes only `pub` API: `remanence_api::pb`
   (`pub mod`), the tonic-generated `DaemonServer`/`CatalogServer`, and
   `ApiState::{new, daemon_service, catalog_service}` (all `pub`). No private-field
   access. Pass.
2. **`!Send` in threading/async** — `serve()` compiling proves
   `DaemonService`/`CatalogService` satisfy tonic's `Send + Sync + 'static` service
   bounds; the `shutdown` future is `Send + 'static`; the bin runs `serve()` via
   `block_on` (no `!Send` value crosses a spawn). Pass.
3. **Reactor-registration timing** — `UnixListener::bind` and `UnixStream::connect`
   are `async` and run inside the runtime (`serve`/`connect_unix` are async;
   `connect_unix`'s `UnixStream::connect` is inside the `service_fn` future the
   channel drives). Pass.
4. **Borrowed-handle plumbing** — none: `ApiState` and the service values are owned
   and moved into the server; no `&'a mut` child handles. Pass.
5. **Trait/method visibility traps** — confirmed reachable cross-crate (see #1);
   no `pub(crate)` widening needed. Pass.

**Finding:** the UDS client connector requires adding `tower` (`service_fn`) and
`hyper-util` (`TokioIo`) as direct deps of `remanence-cli` — both already resolve
in `Cargo.lock` (tower 0.5.3, hyper-util 0.1.20) as transitive deps of tonic 0.14.5.
