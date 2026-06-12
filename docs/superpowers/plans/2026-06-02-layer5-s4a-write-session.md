# Layer 5 S4a — Single-Object Write Session Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rem-daemon` accept a pool-targeted write session, stream one object in, write it to tape via the shared core, and return the canonical locator — behind a single writer thread.

**Architecture:** A dedicated writer thread owns the mutable `CatalogIndex` (Send/!Sync) + `DiscoveryReport` + allowlist; async gRPC handlers dispatch `WriteCommand`s to it over a `tokio::mpsc` channel (`Sender` in `ApiState`) and await `oneshot` replies. Global single-writer: one active session, second `Open` → `FAILED_PRECONDITION`. `AppendObject` streams to a bounded private spool, then the thread calls the existing `write_to_selected_tape` core. Inline (proto `AppendObject` returns `ObjectRecord`).

**Tech Stack:** Rust, `tonic 0.14`, `tokio` (`mpsc`/`oneshot`/`signal`), `remanence-api` (`pool_write` core, `ApiState`, `WriteSessionApi`), `remanence-library` (`DriveHandle`/`LibraryHandle`/`discover`), `remanence-daemon`, `remanence-cli`.

Spec: `docs/layer5-s4a-write-session-design-v0.1.md`. The writer-thread architecture (Send across spawn, `DriveHandle`-held-across-session borrow, `tokio::mpsc::Sender` Send+Sync) was `cargo check` + clippy verified on 2026-06-02 (skeleton since removed; this plan recreates it). Builds on the committed/working-tree S1 daemon.

**Gates (before every commit):** `cargo fmt --all`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p <crate touched>`.

**Test reality:** the writer thread mounts a real drive on `Open`, so its full path is **hardware-gated** (covered by the manual harness e2e, Task 7). Hardware-free automated tests cover: the relocated mount bridge (compile/regression), `expected_content_sha256` verify in the core (`VecBlockSink`), the bounded-spool size cap, and the daemon's pre-mount validation (`AppendObject` with no active session → error). The object-write + locator itself is already covered by `pool_write`'s `VecBlockSink` tests.

---

## File Structure

- `crates/remanence-api/src/mount.rs` (new) — the relocated `load_tape_by_uuid` + `LoadByUuidError` (Task 1).
- `crates/remanence-api/src/pool_write.rs` — `expected_content_sha256` field + verify (Task 2).
- `crates/remanence-api/src/write_owner.rs` (new) — `WriteCommand`, `spawn_writer`, `writer_loop`, the bounded spool helper (Tasks 3, 5).
- `crates/remanence-api/src/lib.rs` — `ApiState.write_tx` + `with_write_owner`; rewrite `WriteSessionApi` handlers (Tasks 4, 5).
- `crates/remanence-cli/src/pool_ops.rs` + `lib.rs` — call `remanence_api::load_tape_by_uuid` (Task 1).
- `crates/remanence-daemon/src/lib.rs` + `main.rs` — discovery + `with_write_owner` + register `WriteSessionServiceServer` (Task 6).

---

### Task 1: Relocate the mount bridge `load_tape_by_uuid` (cli → api)

The owner thread lives in `remanence-api` and cannot call `remanence-cli::pool_ops::load_tape_by_uuid`. Move it (and `LoadByUuidError`) into `remanence-api`; re-point the CLI.

**Files:**
- Create: `crates/remanence-api/src/mount.rs`
- Modify: `crates/remanence-api/src/lib.rs` (add `mod mount;` + `pub use`), `crates/remanence-cli/src/pool_ops.rs` (remove the fn, import from api)

- [ ] **Step 1: Move the code**

Cut `pub fn load_tape_by_uuid<'a>(…)` and `pub enum LoadByUuidError { … }` (+ its `Display`/`From<StateError>` impls) from `crates/remanence-cli/src/pool_ops.rs` into a new `crates/remanence-api/src/mount.rs`. Adjust imports to api's crate paths (it already depends on `remanence-library` + `remanence-state`): `use remanence_library::{resolve_load_target, AccessPolicy, DriveHandle, LibraryHandle, LoadError, LoadPlan, OpenError}; use remanence_state::{CatalogIndex, StateError};`. Use api's `bytes_to_hex` (or inline a hex helper if api lacks one — `pool_write` already imports `sha2`; a 3-line hex fn is fine).

- [ ] **Step 2: Export from api**

In `crates/remanence-api/src/lib.rs`, add near the other `mod` lines:
```rust
mod mount;
```
and with the other `pub use`s:
```rust
pub use mount::{load_tape_by_uuid, LoadByUuidError};
```

- [ ] **Step 3: Re-point the CLI**

In `crates/remanence-cli/src/pool_ops.rs`, delete the moved items and add `use remanence_api::{load_tape_by_uuid, LoadByUuidError};` (merge into the existing `remanence_api` use). All existing call sites (`run_archive_write`/`read`/`verify`) keep compiling unchanged.

- [ ] **Step 4: Verify (regression)**

Run: `cargo test -p remanence-cli && cargo test -p remanence-api`
Expected: PASS — the A.9/B.7 CLI tests still green; api compiles with the relocated bridge.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/mount.rs crates/remanence-api/src/lib.rs crates/remanence-cli/src/pool_ops.rs
git commit -m "Relocate load_tape_by_uuid mount bridge cli -> api (Layer 5 S4a)"
```

---

### Task 2: `expected_content_sha256` verify-before-tape in the write core

**Files:**
- Modify: `crates/remanence-api/src/pool_write.rs`

- [ ] **Step 1: Write the failing test**

Add to `pool_write.rs`'s `#[cfg(test)] mod tests` (use the module's existing `VecBlockSink` + selected-tape test helpers — mirror an existing `write_to_selected_tape` test for setup):
```rust
#[test]
fn write_rejects_expected_content_sha256_mismatch_before_writing() {
    let (mut index, pool_cfg, selected) = /* mirror an existing write_to_selected_tape test's setup */;
    let mut sink = remanence_library::VecBlockSink::default();
    let request = WriteObjectToPoolRequest {
        pool_id: pool_cfg.id.clone(),
        source_path: /* a temp file with known bytes, as existing tests build */,
        archive_path: std::path::PathBuf::from("f.bin"),
        caller_object_id: "c1".to_string(),
        expected_content_sha256: Some([0u8; 32]), // deliberately wrong
    };
    let err = write_to_selected_tape(&mut index, &mut sink, &pool_cfg, request, selected)
        .expect_err("hash mismatch must abort");
    assert!(matches!(err, PoolWriteError::ContentHashMismatch { .. }));
    assert!(sink.blocks().is_empty(), "nothing may be written to tape on mismatch");
}
```
(Match `VecBlockSink`'s real accessor for written blocks — grep `block_io.rs`; adjust `sink.blocks()` accordingly.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p remanence-api write_rejects_expected_content_sha256_mismatch_before_writing`
Expected: FAIL — the field and error variant don't exist.

- [ ] **Step 3: Add the field + error + check**

In `WriteObjectToPoolRequest` (`pool_write.rs:56`), add:
```rust
    /// Optional caller-supplied expected payload SHA-256; verified against the
    /// computed hash before any tape write. `None` = no check.
    pub expected_content_sha256: Option<[u8; 32]>,
```
Add a `PoolWriteError` variant:
```rust
    /// The computed content hash did not match the caller's expected hash.
    ContentHashMismatch { expected: [u8; 32], actual: [u8; 32] },
```
(Update its `Display` arm.) In `write_to_selected_tape`, right after `let prepared = prepare_pool_object(&request, block_size)?;`, before the parity branch:
```rust
    if let Some(expected) = request.expected_content_sha256 {
        if expected != prepared.content_sha256 {
            return Err(PoolWriteError::ContentHashMismatch {
                expected,
                actual: prepared.content_sha256,
            });
        }
    }
```

- [ ] **Step 4: Fix existing constructors**

Every existing `WriteObjectToPoolRequest { … }` literal (in `write_object_to_pool`, `pool_ops.rs` `run_archive_write`, and tests) needs `expected_content_sha256: None`. Add it. Run `cargo build -p remanence-api -p remanence-cli` and fix each site the compiler flags.

- [ ] **Step 5: Run the test + commit**

Run: `cargo test -p remanence-api write_rejects_expected_content_sha256_mismatch_before_writing` → PASS.
```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/pool_write.rs crates/remanence-cli/src/pool_ops.rs
git commit -m "pool_write: verify expected_content_sha256 before tape write (Layer 5 S4a)"
```

---

### Task 3: The writer thread (`write_owner` module)

**Files:**
- Create: `crates/remanence-api/src/write_owner.rs`
- Modify: `crates/remanence-api/src/lib.rs` (`mod write_owner;`)

- [ ] **Step 1: Create the module (this is the verified skeleton, with real Open/Append bodies)**

Create `crates/remanence-api/src/write_owner.rs`:
```rust
//! Single writer thread (Layer 5 S4a). Owns the mutable CatalogIndex +
//! DiscoveryReport + allowlist; serves one write session at a time.

use std::path::PathBuf;

use remanence_library::{DiscoveryReport, DriveHandleSink, StaticAllowlist};
use remanence_state::{CatalogIndex, RemConfig, TapePoolConfig};
use tokio::sync::{mpsc, oneshot};
use tonic::Status;

use crate::pool_write::{select_tape_in_pool, write_to_selected_tape, WriteObjectToPoolRequest};
use crate::{load_tape_by_uuid, pb, TapeUuid};

pub(crate) enum WriteCommand {
    Open {
        pool_id: String,
        library_serial: String,
        reply: oneshot::Sender<Result<pb::WriteSession, Status>>,
    },
    AppendFinish {
        spool_path: PathBuf,
        archive_path: PathBuf,
        caller_object_id: String,
        expected_content_sha256: Option<[u8; 32]>,
        reply: oneshot::Sender<Result<pb::ObjectRecord, Status>>,
    },
    Close { reply: oneshot::Sender<Result<pb::WriteSession, Status>> },
    Abort { reply: oneshot::Sender<Result<pb::WriteSession, Status>> },
    Get { reply: oneshot::Sender<Result<pb::WriteSession, Status>> },
}

pub(crate) struct WriteOwnerConfig {
    pub report: DiscoveryReport,
    pub policy: StaticAllowlist,
    pub pool_configs: std::collections::HashMap<String, TapePoolConfig>,
}

/// Spawn the single writer thread; returns the command sender for ApiState.
pub(crate) fn spawn_writer(index: CatalogIndex, cfg: WriteOwnerConfig) -> mpsc::Sender<WriteCommand> {
    let (tx, rx) = mpsc::channel::<WriteCommand>(16);
    std::thread::Builder::new()
        .name("rem-writer".to_string())
        .spawn(move || writer_loop(index, cfg, rx))
        .expect("spawn writer thread");
    tx
}

fn writer_loop(mut index: CatalogIndex, cfg: WriteOwnerConfig, mut rx: mpsc::Receiver<WriteCommand>) {
    while let Some(cmd) = rx.blocking_recv() {
        match cmd {
            WriteCommand::Open { pool_id, library_serial, reply } => {
                let pool_cfg = match cfg.pool_configs.get(pool_id.trim()) {
                    Some(p) => p.clone(),
                    None => { let _ = reply.send(Err(Status::invalid_argument(format!("unknown pool {pool_id}")))); continue; }
                };
                // Eligibility-only selection at Open (capacity is placeholder; S4e).
                let selected = match select_tape_in_pool(&index, &pool_cfg, 0, &std::collections::HashSet::new()) {
                    Ok(s) => s,
                    Err(e) => { let _ = reply.send(Err(Status::resource_exhausted(format!("select tape: {e}")))); continue; }
                };
                let lib = match cfg.report.library(&library_serial) {
                    Some(lib) => lib,
                    None => { let _ = reply.send(Err(Status::not_found(format!("library {library_serial}")))); continue; }
                };
                let mut library = match lib.open(&cfg.policy) {
                    Ok(h) => h,
                    Err(e) => { let _ = reply.send(Err(Status::internal(format!("open library: {e}")))); continue; }
                };
                let tape_uuid: TapeUuid = selected.tape_uuid;
                let mut drive = match load_tape_by_uuid(&index, &mut library, &cfg.policy, &tape_uuid) {
                    Ok(d) => d,
                    Err(e) => { let _ = reply.send(Err(Status::internal(format!("mount tape: {e}")))); continue; }
                };
                let _ = reply.send(Ok(open_session_proto(&tape_uuid, &pool_id)));

                // Per-session inner loop: `drive` (borrowing `library`) held here.
                while let Some(inner) = rx.blocking_recv() {
                    match inner {
                        WriteCommand::AppendFinish { spool_path, archive_path, caller_object_id, expected_content_sha256, reply } => {
                            let request = WriteObjectToPoolRequest {
                                pool_id: pool_cfg.id.clone(),
                                source_path: spool_path.clone(),
                                archive_path,
                                caller_object_id,
                                expected_content_sha256,
                            };
                            let mut sink = DriveHandleSink(&mut drive);
                            let result = write_to_selected_tape(&mut index, &mut sink, &pool_cfg, request, selected.clone());
                            let _ = std::fs::remove_file(&spool_path);
                            let _ = reply.send(result.map(|r| r.object.to_proto()).map_err(crate::write_owner::status_from_pool_write_error));
                        }
                        WriteCommand::Close { reply } => { let _ = reply.send(Ok(closed_session_proto(&tape_uuid, &pool_id))); break; }
                        WriteCommand::Abort { reply } => { let _ = reply.send(Ok(aborted_session_proto(&tape_uuid, &pool_id))); break; }
                        WriteCommand::Open { reply, .. } => { let _ = reply.send(Err(Status::failed_precondition("write session already active"))); }
                        WriteCommand::Get { reply } => { let _ = reply.send(Ok(open_session_proto(&tape_uuid, &pool_id))); }
                    }
                }
            }
            WriteCommand::Get { reply } => { let _ = reply.send(Err(Status::not_found("no active write session"))); }
            WriteCommand::AppendFinish { reply, .. } => { let _ = reply.send(Err(Status::failed_precondition("no active write session"))); }
            WriteCommand::Close { reply } | WriteCommand::Abort { reply } => { let _ = reply.send(Err(Status::not_found("write session"))); }
        }
    }
}

pub(crate) fn status_from_pool_write_error(err: crate::pool_write::PoolWriteError) -> Status {
    use crate::pool_write::PoolWriteError;
    match err {
        PoolWriteError::ContentHashMismatch { .. } => Status::failed_precondition(err.to_string()),
        // map other variants to invalid_argument / failed_precondition / internal per the design taxonomy
        other => Status::internal(other.to_string()),
    }
}

fn open_session_proto(tape_uuid: &[u8; 16], pool_id: &str) -> pb::WriteSession { build_session(tape_uuid, pool_id, pb::write_session::State::Open) }
fn closed_session_proto(tape_uuid: &[u8; 16], pool_id: &str) -> pb::WriteSession { build_session(tape_uuid, pool_id, pb::write_session::State::Closed) }
fn aborted_session_proto(tape_uuid: &[u8; 16], pool_id: &str) -> pb::WriteSession { build_session(tape_uuid, pool_id, pb::write_session::State::Aborted) }

fn build_session(tape_uuid: &[u8; 16], _pool_id: &str, state: pb::write_session::State) -> pb::WriteSession {
    pb::WriteSession { tape_uuid: tape_uuid.to_vec(), state: state as i32, body_format: "rem-tar-v1".to_string(), ..Default::default() }
}
```
Notes for the implementer: confirm `PoolWriteResult`'s field is `.object` (it is — `PoolWriteResult { object, .. }`) and `pb::write_session::State` / `pb::WriteSession` field names against `pb`; flesh `status_from_pool_write_error`'s `other` arm per the §error-taxonomy in the design (map `SelectTapeError`/writability → `resource_exhausted`/`failed_precondition`). Add `mod write_owner;` to `lib.rs`.

- [ ] **Step 2: Build + Send/Sync assertion test**

Add to a `#[cfg(test)]` in `write_owner.rs`:
```rust
#[test]
fn channel_and_command_bounds_hold() {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>() {}
    assert_send_sync::<tokio::sync::mpsc::Sender<super::WriteCommand>>();
    assert_send::<super::WriteCommand>();
}
```
Run: `cargo test -p remanence-api channel_and_command_bounds_hold` → PASS (and the crate compiles — the writer-thread borrow structure + Send-across-spawn type-check).

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/write_owner.rs crates/remanence-api/src/lib.rs
git commit -m "Add single writer thread + WriteCommand channel (Layer 5 S4a)"
```

---

### Task 4: `ApiState.write_tx` + `with_write_owner` + lifecycle handlers

**Files:**
- Modify: `crates/remanence-api/src/lib.rs`

- [ ] **Step 1: Add the field + constructor**

In `struct ApiState`, add:
```rust
    write_tx: Option<tokio::sync::mpsc::Sender<crate::write_owner::WriteCommand>>,
```
Set `write_tx: None` in `new_with_pool_configs`. Add a constructor that spawns the writer (called once by the daemon):
```rust
    /// Build service state with a live single-writer thread (write daemon).
    pub fn with_write_owner(
        index: CatalogIndex,
        config: &RemConfig,
        report: remanence_library::DiscoveryReport,
        policy: remanence_library::StaticAllowlist,
    ) -> Self {
        let pool_configs: std::collections::HashMap<String, TapePoolConfig> = config
            .tape_pools
            .iter()
            .map(|p| (p.id.trim().to_string(), p.clone()))
            .collect();
        let index_path = index.path().to_path_buf();
        let write_tx = crate::write_owner::spawn_writer(
            index,
            crate::write_owner::WriteOwnerConfig { report, policy, pool_configs: pool_configs.clone() },
        );
        let mut state = Self::new_with_pool_configs_inner(index_path, pool_configs);
        state.write_tx = Some(write_tx);
        state
    }
```
Refactor `new_with_pool_configs` to share construction via a small `new_with_pool_configs_inner(index_path, pool_configs_map)` (so `with_write_owner` doesn't re-open the index — it already moved the index to the thread and kept only the path). Add a `write_session_service(&self) -> WriteSessionApi` vendor mirroring `catalog_service`.

- [ ] **Step 2: Rewrite the lifecycle handlers (Open/Close/Abort/Get)**

Replace `WriteSessionApi`'s `open_write_session`/`close_write_session`/`abort_write_session`/`get_write_session` bodies to dispatch to the writer thread. Add a private helper on `WriteSessionApi`:
```rust
    async fn dispatch_session(
        &self,
        make: impl FnOnce(tokio::sync::oneshot::Sender<Result<pb::WriteSession, Status>>) -> crate::write_owner::WriteCommand,
    ) -> Result<Response<pb::WriteSession>, Status> {
        let tx = self.state.write_tx.as_ref()
            .ok_or_else(|| Status::unavailable("daemon has no write owner (read-only mode)"))?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx.send(make(reply_tx)).await.map_err(|_| Status::internal("writer thread unavailable"))?;
        let session = reply_rx.await.map_err(|_| Status::internal("writer thread dropped reply"))??;
        Ok(Response::new(session))
    }
```
Then e.g. `open_write_session` extracts `pool_target.pool_id` + `library_uuid` (→ `library_serial`), rejecting non-pool targets / `recover_session_id` with `Status::unimplemented`, and calls `self.dispatch_session(|reply| WriteCommand::Open { pool_id, library_serial, reply }).await`. `close`/`abort`/`get` map their request's `session_id` similarly (the global single-writer validates active-session, so `session_id` is advisory in S4a — accept any, document it).

- [ ] **Step 3: Build**

Run: `cargo build -p remanence-api`
Expected: compiles (handlers dispatch; `AppendObject` still the mock until Task 5).

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
cargo clippy -p remanence-api --all-targets -- -D warnings
git add crates/remanence-api/src/lib.rs
git commit -m "ApiState write owner + write-session lifecycle handlers (Layer 5 S4a)"
```

---

### Task 5: Streaming `AppendObject` → bounded private spool → core

**Files:**
- Modify: `crates/remanence-api/src/write_owner.rs` (spool helper), `crates/remanence-api/src/lib.rs` (`append_object` handler)

- [ ] **Step 1: Write the failing test (spool cap)**

Add to `write_owner.rs` tests:
```rust
#[tokio::test]
async fn spool_enforces_size_cap() {
    let dir = tempdir_helper(); // mirror existing temp-dir usage in api tests
    let mut spool = super::Spool::create(dir.path(), 4).expect("create spool"); // cap = 4 bytes
    assert!(spool.write_chunk(b"ab").is_ok());
    assert!(spool.write_chunk(b"cde").is_err(), "exceeding the cap must error");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p remanence-api spool_enforces_size_cap`
Expected: FAIL — `Spool` doesn't exist.

- [ ] **Step 3: Implement the bounded private spool**

Add to `write_owner.rs`:
```rust
pub(crate) struct Spool { file: std::fs::File, path: PathBuf, written: u64, cap: u64 }

impl Spool {
    /// Create a private spool file (0700 dir assumed; file create_new) with a byte cap.
    pub(crate) fn create(dir: &std::path::Path, cap: u64) -> std::io::Result<Self> {
        let path = dir.join(format!("spool-{}.bin", uuid::Uuid::new_v4()));
        let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?;
        Ok(Self { file, path, written: 0, cap })
    }
    pub(crate) fn write_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        self.written = self.written.saturating_add(bytes.len() as u64);
        if self.written > self.cap {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "spool size cap exceeded"));
        }
        self.file.write_all(bytes)
    }
    pub(crate) fn finish(mut self) -> std::io::Result<PathBuf> { use std::io::Write as _; self.file.flush()?; Ok(self.path) }
    pub(crate) fn path(&self) -> &std::path::Path { &self.path }
}
```
(The daemon creates the spool dir `0700` once at startup — Task 6 — and passes it in.)

- [ ] **Step 4: Implement the `append_object` handler**

Replace `WriteSessionApi::append_object` to: read the inbound stream; first message must be `Start` (else `invalid_argument`); create a `Spool` (cap = `declared_size_bytes` if non-zero else a config max constant, e.g. `const SPOOL_MAX_BYTES: u64`); for each `Chunk` `spool.write_chunk(&data)` (cap exceed → `Status::resource_exhausted`); on `Finish` `spool.finish()` then `dispatch` an `AppendFinish` to the writer thread and return its `ObjectRecord`. Sketch:
```rust
async fn append_object(&self, request: Request<tonic::Streaming<pb::AppendObjectMessage>>) -> Result<Response<pb::ObjectRecord>, Status> {
    let tx = self.state.write_tx.as_ref().ok_or_else(|| Status::unavailable("read-only mode"))?;
    let mut stream = request.into_inner();
    // 1) Start
    let start = match stream.message().await?.and_then(|m| m.payload) {
        Some(pb::append_object_message::Payload::Start(s)) => s,
        _ => return Err(Status::invalid_argument("first AppendObject message must be Start")),
    };
    let cap = if start.declared_size_bytes > 0 { start.declared_size_bytes } else { crate::write_owner::SPOOL_MAX_BYTES };
    let mut spool = crate::write_owner::Spool::create(self.state.spool_dir(), cap).map_err(|e| Status::internal(format!("spool: {e}")))?;
    let mut finish: Option<pb::AppendObjectFinish> = None;
    // 2) Chunks until Finish
    while let Some(msg) = stream.message().await? {
        match msg.payload {
            Some(pb::append_object_message::Payload::Chunk(c)) =>
                spool.write_chunk(&c.data).map_err(|_| Status::resource_exhausted("object exceeds spool size cap"))?,
            Some(pb::append_object_message::Payload::Finish(f)) => { finish = Some(f); break; }
            _ => return Err(Status::invalid_argument("expected Chunk or Finish")),
        }
    }
    let finish = finish.ok_or_else(|| Status::invalid_argument("stream ended before Finish"))?;
    let spool_path = spool.finish().map_err(|e| Status::internal(format!("spool flush: {e}")))?;
    let expected = (!finish.expected_content_sha256.is_empty())
        .then(|| <[u8;32]>::try_from(finish.expected_content_sha256.as_slice()))
        .transpose().map_err(|_| Status::invalid_argument("expected_content_sha256 must be 32 bytes"))?;
    let archive_path = start.caller_metadata.get("path").map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(format!("{}.bin", start.caller_object_id)));
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    tx.send(crate::write_owner::WriteCommand::AppendFinish {
        spool_path, archive_path, caller_object_id: start.caller_object_id,
        expected_content_sha256: expected, reply: reply_tx,
    }).await.map_err(|_| Status::internal("writer unavailable"))?;
    let record = reply_rx.await.map_err(|_| Status::internal("writer dropped reply"))??;
    Ok(Response::new(record))
}
```
Add `const SPOOL_MAX_BYTES: u64 = …;` to `write_owner.rs` and a `spool_dir(&self) -> &Path` accessor on `ApiState` (a path stored at `with_write_owner` time). Confirm `pb::append_object_message::Payload` variant names against `pb`.

- [ ] **Step 5: Run tests + commit**

Run: `cargo test -p remanence-api spool_enforces_size_cap` → PASS.
```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/write_owner.rs crates/remanence-api/src/lib.rs
git commit -m "Streaming AppendObject -> bounded private spool -> write core (Layer 5 S4a)"
```

---

### Task 6: Daemon — discovery, write owner, register WriteSessionService

**Files:**
- Modify: `crates/remanence-daemon/src/lib.rs`, `crates/remanence-daemon/src/main.rs`, `crates/remanence-daemon/Cargo.toml`

- [ ] **Step 1: Register the service in `serve`**

`serve` already takes `ApiState`. Add the write service registration:
```rust
        .add_service(pb::write_session_service_server::WriteSessionServiceServer::new(
            state.write_session_service(),
        ))
```
(after the catalog service). No signature change — `serve` is agnostic to whether `write_tx` is set; a read-only `ApiState` simply returns `unavailable` from write RPCs.

- [ ] **Step 2: Build the write owner in `main`**

Add `remanence-library = { path = "../remanence-library" }` to `crates/remanence-daemon/Cargo.toml`. In `main.rs`, replace `ApiState::new_with_config(index, &config)` with discovery + allowlist + `with_write_owner`, and create the spool dir:
```rust
    let report = match remanence_library::discover() {
        Ok(report) => report,
        Err(error) => { eprintln!("error: discover libraries: {error}"); return ExitCode::from(1); }
    };
    let mut policy = remanence_library::StaticAllowlist::new(config.libraries.iter().map(|l| l.serial.clone()));
    for lib in &config.libraries {
        if lib.allow_derived_drive_identity { policy = policy.with_derived_allowed(lib.serial.clone()); }
    }
    let state = remanence_api::ApiState::with_write_owner(index, &config, report, policy);
```
(Confirm `config.libraries`'s field names — `serial`, `allow_derived_drive_identity` — against `RemConfig`. The spool dir defaults under `config.daemon.state_dir.join("spool")`, created `0700` at startup and passed into `with_write_owner`.)

- [ ] **Step 3: Build + commit**

Run: `cargo build -p remanence-daemon` → compiles (the daemon now discovers hardware + serves the write service).
```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-daemon/
git commit -m "rem-daemon: discover hardware + serve WriteSessionService (Layer 5 S4a)"
```

---

### Task 7: Full verification + manual harness e2e

- [ ] **Step 1: Full-workspace gates**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: all pass (new tests: `write_rejects_expected_content_sha256_mismatch_before_writing`, `channel_and_command_bounds_hold`, `spool_enforces_size_cap`; the relocated mount bridge keeps A.9/B.7 green).

- [ ] **Step 2: Manual hardware e2e (akash QuadStor fixture)**

```bash
cargo build --release -p remanence-daemon
sudo setcap cap_sys_rawio+ep target/release/rem-daemon
target/release/rem-daemon --config /var/lib/replica/rem/config.toml --allow 7CBAD9CF74 &
```
Then drive a write session via a gRPC client over the socket (Sutradhara write client, or `grpcurl`): `OpenWriteSession{pool_target{pool_id="scenario-a", mount_if_needed=true}, body_format="rem-tar-v1"}` → stream `AppendObject` for a known file → expect an `ObjectRecord` locator. Verify: (a) a second concurrent `OpenWriteSession` returns `FAILED_PRECONDITION`; (b) the returned locator is **byte-identical** to `rem-debug archive write --pool scenario-a --file <same>` for the same input (cross-transport parity); (c) `rem catalog --endpoint unix:<sock> tapes`/an `EnumerateObjects` finds the object by the same `locator_key`.

- [ ] **Step 3: Record the result in the journal.**

---

## Self-Review

**Spec coverage** (against `docs/layer5-s4a-write-session-design-v0.1.md`):
- Single writer thread + global-single-writer reject → Task 3 (`writer_loop`, second-Open arm). ✓
- tokio-mpsc + blocking_recv + oneshot → Task 3. ✓
- Mount-bridge relocation → Task 1. ✓
- Bounded private spool → Task 5 (`Spool`, cap → `resource_exhausted`). ✓
- Real-core wiring + locator parity → Task 3 (`write_to_selected_tape` → `.object.to_proto()`). ✓
- `expected_content_sha256` verify-before-tape → Task 2 (core) + Task 5 (plumb from `Finish`). ✓
- Lifecycle Open/Append/Close/Abort/Get → Tasks 4, 5. ✓
- Daemon hardware bring-up + register service → Task 6. ✓
- Error taxonomy → `status_from_pool_write_error` (Task 3) + handler mappings (Tasks 4, 5). ✓
- Pinned contract (pool target only; others `unimplemented`) → Task 4 Step 2. ✓
- Acceptance: unit (mismatch/spool/bounds) + manual e2e (parity) → Tasks 2, 3, 5, 7. ✓
- OUT (checkpoint/idempotency/resume/rollover/capacity/async) → not in any task, by design. ✓

**Placeholder scan:** code steps carry real code. The "confirm against `pb`/`RemConfig`/`VecBlockSink` field names" notes are concrete grep-then-adjust instructions (the generated `pb` names can't be hand-verified without the build), not deferred logic. No TBD/TODO in logic.

**Type consistency:** `WriteCommand` variants identical across Task 3 (def), Task 4 (`Open`), Task 5 (`AppendFinish`). `WriteObjectToPoolRequest.expected_content_sha256: Option<[u8;32]>` consistent across Tasks 2, 3, 5. `Spool::{create,write_chunk,finish,path}` consistent across Tasks 5 steps. `ApiState.write_tx` + `with_write_owner` + `write_session_service` consistent across Tasks 4, 5, 6. `spawn_writer(index, WriteOwnerConfig) -> Sender` consistent (Task 3 def, Task 4 call).

**Verified:** the writer-thread architecture (Send across spawn, `DriveHandle`-held-across-session borrow, `tokio::mpsc::Sender` Send+Sync, `blocking_recv`) was cargo-check + clippy clean on 2026-06-02; `ApiState`/`pb`/`pool_write`/`load_tape_by_uuid`/`discover` shapes confirmed at current HEAD.
