# Layer 5 S6b — LibraryService mutations (RefreshInventory + MoveMedium/LoadDrive/UnloadDrive) Design v0.1

Status: design decision. S1/S4a/S5a/S3a/S6a are live. S6b is the **write half**
of LibraryService for same-library robotics: `RefreshInventory` (re-read the
changer and republish the inventory snapshot) + `MoveMedium` / `LoadDrive` /
`UnloadDrive`. It rides the S3a operations backbone (every RPC returns
`OperationRef`, fire-and-track) and dispatches real work to the S4a/S5a/S3a
drive-session owner. `ImportElement`/`ExportElement` (IE-port vendor semantics)
and live `DRIVE_STATUS_BUSY` are deferred to a follow-up; `StreamLibraryEvents`
is S6c. Grounds in `remanence-library`'s `LibraryHandle`
(`refresh`/`move_medium`/`load`/`unload`/`library`), `remanence-api`
(`ApiState`, the S6a snapshot + projection, the drive-session owner, the S3a
`OperationRegistry`), and `proto/layer5.proto` (`LibraryService`).

## Background — what exists

- **`LibraryHandle` does all the robotics already** (`remanence-library`, owned —
  **no lifetime parameter**, so the owner holds it as a plain stack local):
  - `refresh() -> Result<(), ScsiError>` — re-issues READ ELEMENT STATUS,
    preserves host-side data when the RES shape is unchanged, sets dirty on a
    shape mismatch (still returns `Ok`). This is RefreshInventory's engine.
  - `move_medium(src, dst, &dyn AccessPolicy) -> Result<(), MoveError>` — MOVE
    MEDIUM with snapshot preflight (`plan_move`), the derived-identity policy
    gate, `Started`/`Finished`/`Refused` Layer-2 audit, and dirty-state tracking
    (IE-port vendor semantics; `CompletionUnknown` on transport ambiguity).
  - `load(slot, bay, &dyn AccessPolicy) -> Result<(), LoadError>` — composed MOVE
    then SSC LOAD on the drive.
  - `unload(bay, destination: Option<u16>, &dyn AccessPolicy) -> Result<(), UnloadError>`
    — SSC UNLOAD then MOVE bay→destination (or the bay's recorded `source_slot`
    when `destination` is `None`).
  - `library() -> &Library` — read the post-op snapshot back out.
- **S6a static snapshot:** `ApiState.library_snapshot: Option<Arc<LibrarySnapshot>>`
  captured once at startup; `ListLibraries`/`GetLibrary` project it. It is
  **immutable after construction** — nothing can publish an updated inventory.
- **S3a operations backbone:** `OperationRegistry` (ring + broadcast + cancel
  token) + `OperationHandle`; `Catalog.ReconcileTape` is the `fire-and-track`
  template (reserve owner → register → `RequestReceived` → dispatch a
  `SessionCommand` → return `OperationRef`; owner publishes terminal status).
- **The owner** (`write_owner.rs`) serializes hardware on one OS thread, resolves
  a library via `cfg.report.library(serial)` + `lib.open(&cfg.policy)`, and
  already runs `Reconcile` as a fire-and-track command.

## Architecture

Three pieces:

**1. Mutable snapshot cell.** `ApiState.library_snapshot` changes type from
`Option<Arc<LibrarySnapshot>>` to `Option<Arc<RwLock<Arc<LibrarySnapshot>>>>`
(std `RwLock`, no new dependency). A new `ApiState::current_library_snapshot()
-> Option<Arc<LibrarySnapshot>>` helper takes the read lock and clones the inner
`Arc` (S6a's two handlers switch to it — a small, mechanical change). The owner
receives a clone of the **same** cell via `WriteOwnerConfig` and publishes
updated snapshots into it. Read-only daemons stay `None`. Verified Send+Sync (the
cell type and `ApiState`) on 2026-06-03.

**2. Four fire-and-track owner commands** (new `SessionCommand` variants —
`RefreshInventory` / `MoveMedium` / `LoadDrive` / `UnloadDrive`, each carrying the
resolved `library_serial`, the element addresses, and an S3a `OperationHandle`).
Each handler mirrors `reconcile_tape`: reserve the owner (busy →
`FAILED_PRECONDITION`), register the operation, record `RequestReceived`,
`try_send` the command, **return `OperationRef` immediately**.

**3. Uniform owner execution — open → `refresh()` → [act] → publish.** Every
robotics command:
1. resolves the library in `cfg.report` (stable device identity) and
   `lib.open(&cfg.policy)`;
2. calls `library.refresh()` to sync cartridge positions to **current** hardware
   (RES is ~ms; a real MOVE is 8–20 s) — so preflight and the published result
   reflect reality regardless of any drift since startup, and moves **compose**
   correctly across calls;
3. performs the action (`move_medium`/`load`/`unload`; nothing for
   RefreshInventory);
4. publishes `library().clone()` into the cell as a fresh `LibrarySnapshot`
   (new `captured_at`), then records the terminal audit/operation status.

RefreshInventory is simply the no-action case of this sequence.

## The four RPCs

| RPC | request fields | owner action |
|---|---|---|
| `RefreshInventory` | `library_uuid` | `refresh()` then publish |
| `MoveMedium` | `source_element_address`, `destination_element_address` | `move_medium(src, dst, &policy)` |
| `LoadDrive` | `slot_element_address`, `drive_element_address` | `load(slot, bay, &policy)` |
| `UnloadDrive` | `drive_element_address`, `destination_slot_address` | `unload(bay, dest_opt, &policy)` |

`UnloadDrive`'s `destination_slot_address == 0` maps to `None` (the handle homes
the cartridge to its recorded `source_slot`). Element addresses are `u32` on the
wire but `u16` in `LibraryHandle`; the handler narrows them, returning
`invalid_argument` if any exceeds `u16::MAX`.

## Library resolution

`library_uuid` (the S6a `UUIDv5(serial)`) resolves to a `library_serial` by
matching against the current snapshot — reusing the S6a `library_uuid(serial)`
helper. An empty `library_uuid` falls back to `default_library_serial` (as the
S4a write path does). No match / no snapshot → `not_found`. The resolved serial
is passed to the owner command; the owner re-resolves the device via `cfg.report`.

## Single-session + cancellation

A robotics op reserves the single drive-session owner exactly like a read/write/
reconcile session: one at a time, and a robotics op dispatched while any session
is open (or vice-versa) → `FAILED_PRECONDITION`. Cancellation is
**before-dispatch only** — a robot arm mid-MOVE cannot be interrupted; the owner
honors `handle.is_cancelled()` at the same pre-CDB check `reconcile` uses (→
`CANCELLED` "cancelled before dispatch"), and a `CancelOperation` arriving after
the CDB is issued is rejected (the op runs to its terminal state).

## Audit + dirty state

Operation-level audit mirrors `record_reconcile_event`:
`RequestReceived` (QUEUED) → `OperationStarted` (RUNNING) → terminal
`SUCCEEDED`/`FAILED`/`CANCELLED`, with `library_serial` + `src`/`dst` (or
`slot`/`bay`) in the CBOR detail map. (`LibraryHandle` *also* fires its own
Layer-2 `Started`/`Finished`/`Refused` events to its audit hook; wiring that
richer CDB-level stream through the daemon audit log is a deferred enhancement —
S6b records the operation-level lifecycle, which is what the `OperationRef`
contract needs.) On a `CompletionUnknown`-dirty failure (transport ambiguity
mid-MOVE), the op ends `FAILED` and the snapshot may not match reality; the next
`RefreshInventory` resyncs it (the published snapshot carries the last
successful read, so it is never *silently* wrong — `last_inventory_at` dates it).

## Error taxonomy

Owner failures end as a terminal `FAILED` `OperationStatus`; dispatch-time
failures return a `Status` directly. Mapping:

- `MoveError::{AddressUnknown, SameElement}` → `invalid_argument`.
- `MoveError::{SourceEmpty, DestinationFull, DriveBayUnresolved,
  DriveBayMissingDevice, DerivedDriveBay}` → `failed_precondition`.
- `MoveError::ScsiError` → `internal`.
- `LoadError::{NotInLibrary}` → `not_found`; `LoadError::{NoFreeDrive}` →
  `failed_precondition`; `LoadError::Move(e)` recurses the `MoveError` mapping;
  `LoadError::DriveLoad(_)` → `internal`.
- `UnloadError::{OpenDrive, DriveUnload}` → `internal`; `UnloadError::Move(e)`
  recurses.
- `refresh()` `ScsiError` → `internal`.
- owner busy / not present → `failed_precondition` / `unavailable` (as
  `reconcile_tape`).

## Pinned contract for the consumer (orchestrator / harness)

- `LibraryService.{RefreshInventory|MoveMedium|LoadDrive|UnloadDrive}{library_uuid, …}`
  → `OperationRef{operation_id}` immediately. Observe via
  `Daemon.WatchOperation`/`GetOperation`; terminal `SUCCEEDED`/`FAILED`/`CANCELLED`.
- After a `SUCCEEDED` op, `GetLibrary` reflects the new inventory (the owner
  published it); `last_inventory_at` advances.
- One active drive operation at a time: a robotics op during a read/write/
  reconcile session (or vice-versa) → `FAILED_PRECONDITION`.
- Cancellation only takes effect before the changer CDB is issued.

## Scope

**IN (S6b):** the mutable snapshot cell (`Arc<RwLock<Arc<LibrarySnapshot>>>`) +
`ApiState::current_library_snapshot()` (and switching the S6a read handlers to
it); the owner sharing the cell + the open→refresh→act→publish sequence;
`RefreshInventory` + `MoveMedium` + `LoadDrive` + `UnloadDrive` as fire-and-track
owner commands on the S3a backbone; library_uuid→serial resolution;
operation-level audit; the error mapping above.

**OUT:** `ImportElement`/`ExportElement` (IE-port park-vs-vault vendor semantics +
the resulting dirty-state handling) → next slice; live `DRIVE_STATUS_BUSY` /
per-drive live state → next slice; `StreamLibraryEvents` → S6c; richer Layer-2
CDB-level audit-hook wiring through the daemon; mid-MOVE cancellation;
idempotency-key dedup; `rescan()` (full re-discovery) as an RPC; concurrent
multi-drive robotics (Layer 2 dependency); the manual hardware harness e2e.

## Acceptance criteria

1. **Unit:** the u32→u16 element-address narrowing (valid + overflow →
   `invalid_argument`); `destination_slot_address == 0` → `None`;
   library_uuid→serial resolution (match / empty→default / no-match→`not_found`);
   the `MoveError`/`LoadError`/`UnloadError` → `Status` mapping.
2. **Integration (hardware-free):** with no session owner, the four RPCs return
   `unavailable`/`failed_precondition` as appropriate; library_uuid resolution
   and request validation reject malformed inputs before dispatch; a
   `RefreshInventory` against a snapshot-bearing `ApiState` registers an
   operation and returns an `OperationRef`. (The owner's hardware path is
   exercised by the harness, not unit tests.)
3. **Harness e2e (akash fixture, human-run, OUT of Codex scope):** `LoadDrive`
   slot→drive → `WatchOperation` → `SUCCEEDED` → `GetLibrary` shows the drive
   `LOADED`; `UnloadDrive` (default home) → slot repopulated; `MoveMedium`
   slot→slot reflected after `SUCCEEDED`; `RefreshInventory` after a manual mtx
   move shows the change; a robotics op during an open write session →
   `FAILED_PRECONDITION`.
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-api` + `cargo clippy -p remanence-api
--all-targets -- -D warnings` (both clean) on 2026-06-03 with a skeleton
(`crates/remanence-api/src/s6b_skeleton.rs`, since removed — design-only): the
mutable-cell type + `ApiState` Send+Sync asserts, the four owner commands' Send
assert, and the `open → refresh → move_medium/load/unload → library().clone() →
publish into the RwLock cell` sequence type-checked against `LibraryHandle`'s
real signatures.

Five-category result:

1. **Module privacy** — all new items in `remanence-api` beside `ApiState`/the
   owner; they call `pub` `LibraryHandle` methods and read `pub`
   `remanence-library` types. Pass.
2. **`!Send` in threading** — `Arc<RwLock<Arc<LibrarySnapshot>>>` is Send+Sync
   (keeps `ApiState` Send+Sync, shared across async handlers + the owner thread);
   the four `SessionCommand` variants (`String`/`u16`/`Option<u16>`/
   `OperationHandle`) are Send. Pass.
3. **Reactor timing** — no new tokio-aware type; the owner runs on its std
   thread (`blocking_recv`), publishes via a sync `RwLock` write; handlers are
   plain async returning an `OperationRef`. Pass.
4. **Borrowed-handle plumbing** — `LibraryHandle` has **no lifetime parameter**
   (owned), so the owner holds it as a stack local with zero borrow plumbing; the
   cell is `Arc`-shared (owned clones). Pass.
5. **Trait/method visibility traps** — `LibraryHandle::{refresh, move_medium,
   load, unload, library}`, `AccessPolicy`, `Library`, `std::sync::RwLock`, and
   the S3a `OperationHandle`/registry are all reachable from `remanence-api`.
   Pass.

No new dependencies (`std::sync::RwLock`).
