# Layer 5 S3a — Operations + Cancellation (async backbone) Design v0.1

Status: design decision. S1 (catalog) + S4a (write) + S5a (read) are live — the
data plane is done. S3a builds the **async operation backbone** (the lifecycle
the spec §11.3 describes) and wires **`Catalog.ReconcileTape`** as its first
real producer. Grounds in `proto/layer5.proto` (`Daemon` ops + `Catalog.ReconcileTape`),
`remanence-api` (the durable operations projection + `DaemonService`, the
drive-session owner from S4a/S5a), `remanence-state` (audit events + `operations`
table), and `remanence-parity::scan::scan_reconstruct_filemark_map`.

## Background — what exists

- **Durable projection (exists):** audit events → `operations` table; `GetOperation`
  works (`OperationRecord` → `OperationStatus`); `ListOperations` is partial
  (unimplemented filter). The cancellation audit vocabulary exists
  (`CancelRequested`/`CancelledBeforeDispatch`/`CompletedAfterCancel`/
  `CompletionUnknown`/`OperationStarted`/`OperationFailed`).
- **Gaps:** `ListOperations` filter, `CancelOperation`, `WatchOperation` — and a
  **live** representation the durable projection can't provide (streaming progress,
  a cancel signal).
- **No producer:** the only `OperationRef`-returning RPCs (`RefreshInventory`,
  `MoveMedium`, `Load/Unload` — all S6; `Catalog.ReconcileTape`) are unbuilt, so
  nothing creates trackable operations yet.

## Architecture — two representations

| | Durable projection (exists) | Live registry (new in S3a) |
|---|---|---|
| Backing | audit log → `operations` table | in-process `Arc<Mutex<HashMap<OpId, OpEntry>>>` |
| Holds | last committed state; survives restart | progress **ring buffer** (`Vec<OperationStatus>`, last N) + a `tokio::broadcast::Sender<OperationStatus>` + a **cancel token** (`Arc<AtomicBool>`) |
| Serves | `GetOperation` / `ListOperations` | `WatchOperation` (live stream) / `CancelOperation` (signal) |
| On restart | reports last durable state | empty → in-flight op reads from the projection (`UNKNOWN`/`CompletionUnknown`) |

The registry is `Send + Sync` (held in `ApiState`, shared between the async tonic
handlers and the drive-session owner thread). A **producer handle**
(`OperationHandle`, `Send`) given to a running op publishes progress
(push ring + `broadcast.send`) and reads the cancel flag.

## The 4 Daemon RPCs

- **`GetOperation`** — unchanged (durable projection).
- **`ListOperations`** — implement the `filter` map (`kind=`, `state=`, `since=`) + pagination.
- **`WatchOperation`** — snapshot the ring + subscribe to the broadcast (under the
  lock), then stream `OperationStatus` until a terminal state, then close. A small
  forwarding task drains `broadcast::Receiver` → an `mpsc` → `ReceiverStream`
  (avoids the `BroadcastStream` feature). Reconnect replays the ring.
- **`CancelOperation`** — set the cancel token + record a `CancelRequested` audit
  event. The running op honors it at its next safe point → `CANCELLED`
  (`CancelledBeforeDispatch`/`CompletedAfterCancel`); past the last safe point it
  stays `RUNNING` with a cancellation-rejected note.

## `ReconcileTape` — the first async producer

`Catalog.ReconcileTape(tape_uuid)` is the project's first **fire-and-track** RPC:
1. `register()` an operation in the live registry → record `RequestReceived` (QUEUED).
2. Dispatch a `Reconcile{tape_uuid, handle}` command to the **drive-session owner**
   (a third session kind beside read/write) — non-blocking.
3. **Return `OperationRef` immediately** (does not await completion).

The owner picks up `Reconcile`, mounts the tape, runs
`scan_reconstruct_filemark_map`, reconciles the catalog's `tape_files` projection
for that tape against the scanned structure, publishes progress per tape-file via
the handle, and checks `handle.is_cancelled()` at tape-file boundaries (the safe
points) → `SUCCEEDED` / `CANCELLED` / `FAILED`, emitting the matching audit event.
While it runs the owner is busy (single-session); a concurrent `Open*` →
`FAILED_PRECONDITION`.

**Reconcile v1 scope:** scan the filemark/object structure + reconcile the
rebuildable `tape_files` projection, with progress + cancellation. Damage recovery,
parity-aware reconcile, and cross-tape reconcile are deferred.

## State machine & restart

Audit events remain the durable source (`RequestReceived`→QUEUED,
`OperationStarted`→RUNNING, terminal→SUCCEEDED/FAILED/CANCELLED/UNKNOWN); the live
registry mirrors them for streaming/cancel. On restart the registry is empty, so
an op that was in-flight reads its last durable state from the projection (or
`UNKNOWN`); S3a does not auto-resume operations.

## Error taxonomy

`not_found` (unknown op id), `failed_precondition` (cancel past the last safe
point → rejected; owner busy when reconcile dispatched), `invalid_argument`
(malformed op id / filter), `internal` (registry/audit/SCSI). `WatchOperation`
and reconcile failures surface as a terminal `FAILED` `OperationStatus` with
`error_summary`.

## Pinned contract for the consumer (orchestrator / harness)

- `Catalog.ReconcileTape{tape_uuid}` → `OperationRef{operation_id}` (immediate).
- `Daemon.WatchOperation{operation_id}` → `stream OperationStatus` (progress until
  terminal, then close). `Daemon.GetOperation` polls the same. `Daemon.CancelOperation{operation_id}`
  → requests cancellation (honored at the next safe point). `Daemon.ListOperations{filter}`
  with `kind=`/`state=`/`since=`.
- One active drive session at a time: a `ReconcileTape` while a read/write session
  is open (or vice-versa) → `FAILED_PRECONDITION`.

## Scope

**IN (S3a):** the live registry (ring buffer + broadcast + cancel token + producer
handle); `ListOperations` filter; `WatchOperation` streaming + ring replay;
`CancelOperation` (token + audit); `ReconcileTape` as the async producer (scan +
reconcile `tape_files`, on the owner, cancellable). **OUT:** richer reconcile
(damage/parity/cross-tape) → later; operation resume-after-restart; idempotency-key
dedup; the S6 robotics producers (they reuse this backbone); progress-payload
schema beyond simple key/value.

## Acceptance criteria

1. **Unit:** registry (register → publish → terminal; ring replay snapshot; cancel
   token set/observe); `ListOperations` filter (`kind`/`state`/`since`); the
   audit→state-machine mapping.
2. **Integration (hardware-free):** a synthetic op driven through the registry —
   `WatchOperation` yields the published progress then closes on terminal;
   `CancelOperation` flips the token and the synthetic op ends `CANCELLED`.
3. **Harness e2e:** fire `ReconcileTape` on a written tape → `WatchOperation`
   streams progress → `SUCCEEDED`; a run cancelled mid-scan → `CANCELLED`; a
   concurrent `Open*` during reconcile → `FAILED_PRECONDITION`.
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-api` + `cargo clippy -p remanence-api
--all-targets -- -D warnings` (both clean) on 2026-06-03, with a real-body skeleton
(`crates/remanence-api/src/operations_skeleton.rs`: `OperationRegistry`/`OpEntry`/
`OperationHandle`, `WatchOperation` via `broadcast→mpsc→ReceiverStream`, the
`Reconcile` owner command, Send/Sync asserts). Removed after (design-only); the
plan recreates it. Checked against current HEAD (post S1/S4a/S5a).

Five-category result:
1. **Module privacy** — all new items in `remanence-api`; reconcile reuses `pub`
   `remanence_parity::scan::scan_reconstruct_filemark_map` and the existing
   drive-session owner. Pass.
2. **`!Send` in threading** — verified: `OperationRegistry` is Send+Sync (held in
   `ApiState`, shared across async handlers + the owner thread); `OperationHandle`
   is Send (owner thread); the `WatchOperation` forwarding task's future is Send;
   the `Reconcile` command is Send. Pass.
3. **Reactor timing** — `WatchOperation`'s forwarding `tokio::spawn` runs inside
   the handler's runtime; the owner publishes via `broadcast::Sender::send` (sync,
   no runtime needed). Pass.
4. **Borrowed-handle plumbing** — the registry is `Arc`-shared (owned clones), no
   new borrows; reconcile reuses the drive stack-local pattern on the owner. Pass.
5. **Trait/method visibility traps** — `pb::{OperationStatus, OperationState}`,
   `tokio::broadcast`, `ReceiverStream`, `scan_reconstruct_filemark_map` reachable.
   Pass.

No new dependencies (`broadcast` is in tokio `full`; the watch stream avoids the
`BroadcastStream`/`tokio-stream` `sync` feature by forwarding to an `mpsc`).
