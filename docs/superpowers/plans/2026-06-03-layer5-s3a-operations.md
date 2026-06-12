# Layer 5 S3a — Operations + Cancellation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `rem-daemon` track long-running operations — a live registry behind `WatchOperation`/`CancelOperation`/`ListOperations` — and wire `Catalog.ReconcileTape` as the first fire-and-track producer.

**Architecture:** A `Send+Sync` in-memory `OperationRegistry` (per-op progress ring buffer + `tokio::broadcast` + `Arc<AtomicBool>` cancel token) in `ApiState`, alongside the existing durable audit→`operations` projection. `WatchOperation` streams via `broadcast→mpsc→ReceiverStream`; `CancelOperation` flips the token + records `CancelRequested`. `ReconcileTape` registers an op, returns `OperationRef` immediately, and dispatches a `Reconcile` command to the drive-session owner (which scans via `scan_reconstruct_filemark_map`, reconciles `tape_files`, publishes progress, checks the cancel token at tape-file safe points).

**Tech Stack:** Rust, `tonic 0.14`, `tokio` (`broadcast`/`mpsc`/`AtomicBool`), `remanence-api` (`DaemonService`, `write_owner` session owner, `operations` projection), `remanence-parity::scan`, `remanence-state` (audit + `operations` table).

Spec: `docs/layer5-s3a-operations-design-v0.1.md`. The registry Send+Sync, the watch stream, and the `Reconcile`-on-owner command were `cargo check` + clippy verified on 2026-06-03 (skeleton removed; this plan recreates it). Builds on S1/S4a/S5a.

**Gates (before every commit):** `cargo fmt --all`; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo test -p <crate touched>`.

**Test reality:** the registry, `WatchOperation`, `CancelOperation`, `ListOperations` filter, and the state-machine mapping are **hardware-free** (unit/integration with a synthetic op). `ReconcileTape`'s actual scan mounts a drive → hardware-gated (manual harness e2e, Task 6).

---

## File Structure

- `crates/remanence-api/src/operations.rs` (new) — `OperationRegistry`, `OpEntry`, `OperationHandle`, `build_watch_stream`, `ListOperations` filter helper (Tasks 1, 2).
- `crates/remanence-api/src/lib.rs` — `ApiState.operations` field; `Daemon` `list_operations`/`cancel_operation`/`watch_operation`; `Catalog.reconcile_tape` (Tasks 1–3, 5).
- `crates/remanence-api/src/write_owner.rs` — `SessionCommand::Reconcile` + `handle_reconcile` (Task 4).

---

### Task 1: The operation registry + `ApiState.operations`

**Files:**
- Create: `crates/remanence-api/src/operations.rs`
- Modify: `crates/remanence-api/src/lib.rs` (`mod operations;`, `ApiState` field + constructors)

- [ ] **Step 1: Write the failing test**

In `operations.rs` `#[cfg(test)]`:
```rust
#[tokio::test]
async fn registry_publishes_replays_and_cancels() {
    let reg = super::OperationRegistry::default();
    let id = uuid::Uuid::from_u128(1);
    let handle = reg.register(id, "reconcile_tape");
    handle.publish(super::status(id, "reconcile_tape", crate::pb::OperationState::Running, &[("scanned","1")]));
    // a watcher gets the ring replay + the next live update
    let mut stream = reg.watch(&id).expect("watch");
    handle.publish(super::status(id, "reconcile_tape", crate::pb::OperationState::Succeeded, &[]));
    use tokio_stream::StreamExt as _;
    let first = stream.next().await.unwrap().unwrap();   // replayed Running
    assert_eq!(first.state, crate::pb::OperationState::Running as i32);
    let last = stream.next().await.unwrap().unwrap();    // live Succeeded
    assert_eq!(last.state, crate::pb::OperationState::Succeeded as i32);
    assert!(stream.next().await.is_none(), "stream closes on terminal");

    // cancel token
    let id2 = uuid::Uuid::from_u128(2);
    let h2 = reg.register(id2, "reconcile_tape");
    assert!(!h2.is_cancelled());
    reg.request_cancel(&id2).unwrap();
    assert!(h2.is_cancelled());
}
```

- [ ] **Step 2: Run → fail** (`cargo test -p remanence-api registry_publishes_replays_and_cancels` — module/types don't exist).

- [ ] **Step 3: Implement `operations.rs`** (the verified skeleton + a `status` helper):
```rust
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::ReceiverStream, Stream};
use tonic::Status;
use uuid::Uuid;
use crate::pb;

const RING_CAP: usize = 256;
const BROADCAST_CAP: usize = 256;

pub(crate) type OperationStatusStream =
    Pin<Box<dyn Stream<Item = Result<pb::OperationStatus, Status>> + Send + 'static>>;

struct OpEntry { ring: Vec<pb::OperationStatus>, tx: broadcast::Sender<pb::OperationStatus>, cancel: Arc<AtomicBool> }

#[derive(Clone, Default)]
pub(crate) struct OperationRegistry { ops: Arc<Mutex<HashMap<Uuid, OpEntry>>> }

pub(crate) struct OperationHandle { op_id: Uuid, tx: broadcast::Sender<pb::OperationStatus>, cancel: Arc<AtomicBool>, ops: Arc<Mutex<HashMap<Uuid, OpEntry>>> }

impl OperationRegistry {
    pub(crate) fn register(&self, op_id: Uuid, _kind: &str) -> OperationHandle {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAP);
        let cancel = Arc::new(AtomicBool::new(false));
        self.ops.lock().expect("ops lock").insert(op_id, OpEntry { ring: Vec::new(), tx: tx.clone(), cancel: cancel.clone() });
        OperationHandle { op_id, tx, cancel, ops: self.ops.clone() }
    }
    pub(crate) fn request_cancel(&self, op_id: &Uuid) -> Result<(), Status> {
        let ops = self.ops.lock().expect("ops lock");
        ops.get(op_id).ok_or_else(|| Status::not_found("operation"))?.cancel.store(true, Ordering::SeqCst);
        Ok(())
    }
    pub(crate) fn watch(&self, op_id: &Uuid) -> Result<OperationStatusStream, Status> {
        let ops = self.ops.lock().expect("ops lock");
        let entry = ops.get(op_id).ok_or_else(|| Status::not_found("operation"))?;
        let (snapshot, rx) = (entry.ring.clone(), entry.tx.subscribe());
        drop(ops);
        Ok(build_watch_stream(snapshot, rx))
    }
}

impl OperationHandle {
    pub(crate) fn is_cancelled(&self) -> bool { self.cancel.load(Ordering::SeqCst) }
    pub(crate) fn publish(&self, status: pb::OperationStatus) {
        if let Ok(mut ops) = self.ops.lock() {
            if let Some(e) = ops.get_mut(&self.op_id) {
                if e.ring.len() >= RING_CAP { e.ring.remove(0); }
                e.ring.push(status.clone());
            }
        }
        let _ = self.tx.send(status);
    }
}

pub(crate) fn is_terminal(s: &pb::OperationStatus) -> bool {
    matches!(pb::OperationState::try_from(s.state), Ok(pb::OperationState::Succeeded | pb::OperationState::Failed | pb::OperationState::Cancelled))
}

fn build_watch_stream(snapshot: Vec<pb::OperationStatus>, mut rx: broadcast::Receiver<pb::OperationStatus>) -> OperationStatusStream {
    let (tx, out_rx) = tokio::sync::mpsc::channel::<Result<pb::OperationStatus, Status>>(BROADCAST_CAP);
    tokio::spawn(async move {
        for s in snapshot { let t = is_terminal(&s); if tx.send(Ok(s)).await.is_err() { return; } if t { return; } }
        loop {
            match rx.recv().await {
                Ok(s) => { let t = is_terminal(&s); if tx.send(Ok(s)).await.is_err() { return; } if t { return; } }
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });
    Box::pin(ReceiverStream::new(out_rx))
}

#[cfg(test)]
pub(crate) fn status(id: Uuid, kind: &str, state: pb::OperationState, progress: &[(&str,&str)]) -> pb::OperationStatus {
    pb::OperationStatus { operation_id: id.as_bytes().to_vec(), operation_kind: kind.to_string(), state: state as i32, created_at: None, updated_at: None, progress: progress.iter().map(|(k,v)| (k.to_string(), v.to_string())).collect(), error_summary: String::new() }
}
```
(Confirm `pb::OperationState::try_from` exists — prost derives `TryFrom<i32>`; if not, compare against `as i32` as the design skeleton did. Verify `OperationStatus` field set against `pb`.)

Add `mod operations;` to `lib.rs`; add `operations: crate::operations::OperationRegistry` to `ApiState` (it's `Clone + Default`), default-initialize it in every constructor (`new_with_pool_configs`, `with_session_owner`).

- [ ] **Step 4: Run → pass; commit**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/operations.rs crates/remanence-api/src/lib.rs
git commit -m "Add operation registry (ring + broadcast + cancel token) (S3a)"
```

---

### Task 2: `ListOperations` filter

**Files:** `crates/remanence-api/src/operations.rs` (pure filter fn), `crates/remanence-api/src/lib.rs` (handler)

- [ ] **Step 1: Failing test** — a pure `matches_filter(record_kind, record_state, started_at, &filter_map) -> bool`:
```rust
#[test]
fn operation_filter_matches_kind_state_since() {
    use std::collections::HashMap;
    let f = |pairs: &[(&str,&str)]| pairs.iter().map(|(k,v)|(k.to_string(),v.to_string())).collect::<HashMap<_,_>>();
    assert!(super::matches_filter("reconcile_tape","running","2026-06-03T10:00:00Z", &f(&[("kind","reconcile_tape")])));
    assert!(!super::matches_filter("reconcile_tape","running","2026-06-03T10:00:00Z", &f(&[("state","succeeded")])));
    assert!(super::matches_filter("reconcile_tape","running","2026-06-03T10:00:00Z", &f(&[("since","2026-06-03T09:00:00Z")])));
    assert!(!super::matches_filter("reconcile_tape","running","2026-06-03T08:00:00Z", &f(&[("since","2026-06-03T09:00:00Z")])));
}
```

- [ ] **Step 2: Run → fail.**

- [ ] **Step 3: Implement `matches_filter`** (string/RFC3339 compares; unknown filter keys → `false` is too strict — ignore unknown keys, match the known ones) and wire `list_operations`: drop the `if !filter.is_empty()` unimplemented branch; after `list_operations()` from the projection, retain those matching `matches_filter(record.operation_kind, record.state, record.started_at_utc, &request.filter)`; keep `next_page_token: None` (pagination stays simple in S3a — `ensure_unpaged` still rejects page tokens).

- [ ] **Step 4: Run → pass; commit**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/operations.rs crates/remanence-api/src/lib.rs
git commit -m "Implement ListOperations filter (kind/state/since) (S3a)"
```

---

### Task 3: `WatchOperation` + `CancelOperation`

**Files:** `crates/remanence-api/src/lib.rs`

- [ ] **Step 1: Failing test** (in `lib.rs` tests — drive a synthetic op through `ApiState.operations`):
```rust
#[tokio::test]
async fn watch_streams_until_terminal_and_cancel_flips_token() {
    let state = test_api_state(); // existing helper that builds ApiState over a temp index
    let id = uuid::Uuid::from_u128(7);
    let handle = state.operations.register(id, "reconcile_tape");
    let daemon = state.daemon_service();
    // cancel
    use pb::daemon_server::Daemon as _;
    daemon.cancel_operation(tonic::Request::new(pb::CancelOperationRequest { operation_id: id.as_bytes().to_vec(), ..Default::default() })).await.expect("cancel");
    assert!(handle.is_cancelled());
    // watch: publish a terminal status, expect the stream to deliver it and close
    handle.publish(crate::operations::status(id, "reconcile_tape", pb::OperationState::Cancelled, &[]));
    let resp = daemon.watch_operation(tonic::Request::new(pb::GetOperationRequest { operation_id: id.as_bytes().to_vec() })).await.expect("watch");
    use tokio_stream::StreamExt as _;
    let mut s = resp.into_inner();
    let first = s.next().await.unwrap().unwrap();
    assert_eq!(first.state, pb::OperationState::Cancelled as i32);
    assert!(s.next().await.is_none());
}
```
(Use/extend the existing `ApiState` test constructor — grep the `#[cfg(test)]` module for how other Daemon tests build state.)

- [ ] **Step 2: Run → fail** (cancel/watch return `unimplemented`).

- [ ] **Step 3: Implement** in `DaemonService`:
```rust
type WatchOperationStream = crate::operations::OperationStatusStream;

async fn watch_operation(&self, request: Request<pb::GetOperationRequest>) -> Result<Response<Self::WatchOperationStream>, Status> {
    let id = decode_uuid_bytes(&request.into_inner().operation_id, "operation_id")?;
    let stream = self.state.operations.watch(&Uuid::from_bytes(id))?;
    Ok(Response::new(stream))
}

async fn cancel_operation(&self, request: Request<pb::CancelOperationRequest>) -> Result<Response<pb::CancelOperationResponse>, Status> {
    let id = decode_uuid_bytes(&request.into_inner().operation_id, "operation_id")?;
    let op_id = Uuid::from_bytes(id);
    self.state.operations.request_cancel(&op_id)?;
    // record a CancelRequested audit event via the existing operation-audit path.
    self.state.record_cancel_requested(op_id)?;  // add a thin helper mirroring the existing OperationStarted/RequestReceived emit (lib.rs ~2052/2184)
    Ok(Response::new(pb::CancelOperationResponse { /* fields per pb (e.g. accepted: true) */ ..Default::default() }))
}
```
(Confirm the `WatchOperationStream` associated type already declared at the impl — reuse it. Confirm `CancelOperationResponse` fields. Add `record_cancel_requested` next to the existing operation-audit emit helper; if recording requires the single-writer `StateHandle`, route it through the session owner — but per the audit addendum, audit append is daemon-authoritative and may be a direct append; mirror however `OperationStarted` is currently emitted.)

- [ ] **Step 4: Run → pass; commit**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/lib.rs
git commit -m "Implement WatchOperation + CancelOperation over the registry (S3a)"
```

---

### Task 4: `SessionCommand::Reconcile` + `handle_reconcile` on the owner

**Files:** `crates/remanence-api/src/write_owner.rs`

- [ ] **Step 1: Add the command + handler**

Add to `SessionCommand`:
```rust
    Reconcile {
        tape_uuid: [u8; 16],
        handle: crate::operations::OperationHandle,
    },
```
In `session_loop`'s outer match, add `SessionCommand::Reconcile { tape_uuid, handle } => handle_reconcile(&mut index, &cfg, tape_uuid, handle)`. Add (mirroring `handle_open_read`'s mount, but run-to-completion — no inner loop):
```rust
fn handle_reconcile(
    index: &mut CatalogIndex,
    cfg: &WriteOwnerConfig,
    tape_uuid: [u8; 16],
    handle: crate::operations::OperationHandle,
) {
    let op_id = handle.op_id_uuid(); // add an accessor, for status() construction
    handle.publish(running_status(op_id, &[("phase","mount")]));
    // mount
    let lib = match resolve_library_for_tape(cfg, index, &tape_uuid) { Ok(l)=>l, Err(_)=>{ handle.publish(failed_status(op_id, "no library for tape")); return } };
    let mut library = match lib.open(&cfg.policy) { Ok(h)=>h, Err(e)=>{ handle.publish(failed_status(op_id, &format!("open library: {e}"))); return } };
    let mut drive = match crate::load_tape_by_uuid(index, &mut library, &cfg.policy, &tape_uuid) { Ok(d)=>d, Err(e)=>{ handle.publish(failed_status(op_id, &format!("mount: {e}"))); return } };
    // scan the tape structure, checking cancellation at tape-file boundaries
    let scan = {
        let mut source = remanence_library::DriveHandleSource(&mut drive);
        match remanence_parity::scan::scan_reconstruct_filemark_map(&mut source /*, scheme/params per its signature */) {
            Ok(map) => map, Err(e) => { handle.publish(failed_status(op_id, &format!("scan: {e}"))); return }
        }
    };
    if handle.is_cancelled() { handle.publish(cancelled_status(op_id)); return; }
    // reconcile the tape_files projection for this tape against `scan`, emitting per-file progress
    match reconcile_tape_files(index, &tape_uuid, &scan, &handle) {  // checks handle.is_cancelled() between files
        Ok(()) => handle.publish(succeeded_status(op_id, &[("reconciled","ok")])),
        Err(ReconcileExit::Cancelled) => handle.publish(cancelled_status(op_id)),
        Err(ReconcileExit::Failed(msg)) => handle.publish(failed_status(op_id, &msg)),
    }
}
```
Notes: `running_status`/`failed_status`/`cancelled_status`/`succeeded_status` build `pb::OperationStatus` (reuse `operations::status`). Confirm `scan_reconstruct_filemark_map`'s exact signature/params (scheme, bootstrap digest) against `remanence-parity/src/scan.rs:99` and what a no-parity tape needs. `reconcile_tape_files` (new, in this module or `read_core`) compares the scanned filemark map to `index.list_tape_files(tape_uuid)` and updates the projection to match reality, returning `Cancelled` if `handle.is_cancelled()` between files — **v1: scan + reconcile `tape_files`; damage/parity/cross-tape deferred.** Add `OperationHandle::op_id_uuid()` accessor in `operations.rs`.

- [ ] **Step 2: Build** (`cargo build -p remanence-api`) — owner compiles with the Reconcile arm. (Automated coverage of the scan path is hardware-gated; the registry/publish path is covered by Tasks 1/3.)

- [ ] **Step 3: Commit**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/write_owner.rs crates/remanence-api/src/operations.rs
git commit -m "Owner: Reconcile session (scan + tape_files reconcile, cancellable) (S3a)"
```

---

### Task 5: `Catalog.reconcile_tape` — register + dispatch + OperationRef

**Files:** `crates/remanence-api/src/lib.rs`

- [ ] **Step 1: Implement** (replace the `unimplemented` stub):
```rust
async fn reconcile_tape(&self, request: Request<pb::ReconcileTapeRequest>) -> Result<Response<pb::OperationRef>, Status> {
    let req = request.into_inner();
    let tape_uuid = decode_uuid_bytes(&req.tape_uuid, "tape_uuid")?;
    let tx = self.state.session_tx.as_ref().ok_or_else(|| Status::unavailable("no session owner (read-only daemon)"))?;
    let op_id = Uuid::new_v4();
    self.state.record_request_received(op_id, "reconcile_tape")?;  // QUEUED in the durable projection (mirror existing emit)
    let handle = self.state.operations.register(op_id, "reconcile_tape");
    tx.try_send(crate::write_owner::SessionCommand::Reconcile { tape_uuid, handle })
        .map_err(|_| Status::failed_precondition("a drive session is already active"))?;
    Ok(Response::new(pb::OperationRef { operation_id: op_id.as_bytes().to_vec() }))
}
```
(Use `try_send` so a busy owner → `FAILED_PRECONDITION` immediately rather than awaiting; confirm `session_tx` is reachable from `CatalogService` — it holds `state: ApiState`. Add `record_request_received` mirroring the existing operation-audit emit.)

- [ ] **Step 2: CLI parse/dispatch (if needed)** — `reconcile_tape` is a `Catalog` RPC; if a `rem catalog reconcile` client command is wanted for the harness, add it; otherwise the harness/grpcurl drives it directly. (Out of S3a unless the harness needs the CLI client verb — keep minimal.)

- [ ] **Step 3: Build + commit**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/remanence-api/src/lib.rs
git commit -m "Catalog.reconcile_tape: register op + dispatch async reconcile (S3a)"
```

---

### Task 6: Full verification + manual harness e2e

- [ ] **Step 1: Full-workspace gates**
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```
Expected: all pass (new: `registry_publishes_replays_and_cancels`, `operation_filter_matches_kind_state_since`, `watch_streams_until_terminal_and_cancel_flips_token`; S1/S4a/S5a regression green).

- [ ] **Step 2: Manual hardware e2e (akash QuadStor fixture)**

With `rem-daemon` running (S4a/S5a bring-up): write an object to a tape, then drive `Catalog.ReconcileTape{tape_uuid}` via a gRPC client → get an `OperationRef`; `Daemon.WatchOperation{operation_id}` streams progress → `SUCCEEDED`. Repeat and `Daemon.CancelOperation` mid-scan → terminal `CANCELLED`. Attempt an `OpenWriteSession`/`OpenReadSession` while reconcile runs → `FAILED_PRECONDITION`. `Daemon.ListOperations{filter: state=succeeded}` shows the completed reconcile.

- [ ] **Step 3: Record the result in the journal.**

---

## Self-Review

**Spec coverage** (against `docs/layer5-s3a-operations-design-v0.1.md`):
- Live registry (ring + broadcast + cancel token + handle) → Task 1. ✓
- `ListOperations` filter → Task 2. ✓
- `WatchOperation` (stream + ring replay) + `CancelOperation` (token + audit) → Task 3. ✓
- `ReconcileTape` async producer (register → OperationRef → owner reconcile, scan + `tape_files`, cancellable) → Tasks 4, 5. ✓
- Single active session (reconcile vs read/write) → Task 5 (`try_send` → `FAILED_PRECONDITION`). ✓
- Error taxonomy + state machine → Tasks 3, 4, 5. ✓
- OUT (damage/parity reconcile, resume, idempotency-dedup, S6 producers) → not in any task, by design. ✓

**Placeholder scan:** the `reconcile_tape_files` body + `scan_reconstruct_filemark_map`'s exact params are described, not fully coded — flagged as "confirm the signature; v1 = scan + tape_files reconcile" (the scan API + reconcile-projection detail is the one spot the implementer fleshes against `remanence-parity`/`remanence-state`). The `record_request_received`/`record_cancel_requested` audit helpers are "mirror the existing `OperationStarted` emit" — concrete (the emit path exists at `lib.rs:2052/2184`). No logic-free placeholders in the registry/RPC tasks.

**Type consistency:** `OperationRegistry`/`OperationHandle`/`OperationStatusStream`/`status()` consistent across Tasks 1, 3, 4. `ApiState.operations` consistent Tasks 1, 3, 5. `SessionCommand::Reconcile{tape_uuid, handle}` consistent Tasks 4, 5. `is_terminal`/`build_watch_stream` are the verified skeleton.

**Verified:** registry Send+Sync (shared async handlers + owner thread), the `broadcast→mpsc→ReceiverStream` watch stream, and the `Reconcile`-on-owner command were cargo-check + clippy clean on 2026-06-03. `Daemon` stubs, the operation-audit emit path (`lib.rs:2052/2184`), `Catalog.reconcile_tape`, `SessionCommand`/`session_loop`, and `scan_reconstruct_filemark_map` confirmed at current HEAD.
