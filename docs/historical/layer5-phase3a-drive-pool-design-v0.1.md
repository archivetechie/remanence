# Layer 5 — Phase 3a: drive-actor pool + mount orchestration (behavior-preserving) Design v0.1

Status: design decision. Third phase of the multi-drive concurrency architecture
(`docs/multidrive-concurrency-architecture-v0.1.md`), first of its three sub-slices
(3a structural → 3b concurrency → 3c `DRIVE_STATUS_BUSY`). Builds on Phase 1
(owned `DriveHandle`) + Phase 2 (`ChangerHandle`, `Send` handles). Goal: replace
the single owner thread with the **actor pool + async mount orchestration**, while
keeping **one global reservation** so behavior is unchanged (still one session at a
time). 3b flips the reservation to per-drive to turn concurrency on.

## Background — today's single owner

`spawn_session_owner` runs ONE `std::thread` (`session_loop`) that owns the
`CatalogIndex` (`!Send`) and, per session, opens a `LibraryHandle` + mounts a tape
(`load_tape_by_uuid`: changer LOAD + `open_drive`) and then enters a **nested loop
on the single `mpsc`** consuming `AppendFinish`/`Close`/… until the session ends.
`ApiState` holds one `session_tx` + one `session_busy` (`AtomicBool`); ~21 handler
sites dispatch to it. WAL is already on (`index.rs:3256`).

## Architecture — 1 changer actor + N drive actors, async `mount`

**Actors** (each its own `std::thread` + `mpsc`, each builds its own `!Send`
`CatalogIndex` connection in-thread):
- **ChangerActor** owns the `ChangerHandle`. Commands: `Move{src,dst}`,
  `Refresh`, plus the robotics ops (the S6b `RoboticsAction`s) and the reconcile
  mount. Serializes the robot.
- **DriveActor[bay]** owns a `DriveHandle` for its bay (opened at startup, see
  below) + its own catalog connection. Commands: the per-session work for that
  drive — `OpenWrite`/`AppendFinish`/`Close`/`Abort`/`Get`, `OpenRead`/`ReadFile`/
  `CloseRead`/`GetRead`, and `Reconcile` I/O. Each runs the **per-drive in-session
  loop** (today's nested loop, now homed on the drive's own channel).

**Startup** (`with_drive_pool`, replacing `with_session_owner`): open the
`LibraryHandle`; for each installed drive bay `open_drive(bay)` → an owned
`DriveHandle` (Phase 1) and **move it into a spawned DriveActor thread** (Phase 2
made `DriveHandle: Send` — verified); then `library.into_changer()` (Phase 2) and
move the `ChangerHandle` into the ChangerActor thread. The drive's `/dev/sg` fd
persists across mounts — the changer moves *tapes* in/out of the bay; the
DriveActor does SSC/LOAD/READ/WRITE on its persistent handle.

**`DrivePool` registry** in `ApiState` (replaces `session_tx`/`session_busy`),
`Send + Sync + Clone` (verified):
```rust
struct DrivePool {
    changer_tx: mpsc::Sender<ChangerCommand>,
    drives: Arc<HashMap<u16, mpsc::Sender<DriveCommand>>>,   // bay -> drive actor
    reservation: Arc<AtomicBool>,                            // ONE global flag (3a)
    sessions: Arc<Mutex<HashMap<Uuid, u16>>>,                // session_id -> bay
}
```

**Async `mount` orchestration module** (the A-synthesis — remanence owns the
mechanism, on the reactor, no coordinator thread):
- `mount::open_write(state, pool_id) -> WriteSession`: acquire the reservation
  (global in 3a); select the tape (`select_tape_in_pool` over a read-only catalog
  connection); pick the (only free, in 3a) drive bay; `changer_tx.send(Move{slot→bay})`
  + await; `drive_tx.send(OpenWrite{...})` + await; record `session_id → bay`.
- `mount::close/abort`: route to the session's DriveActor (finalize), then
  `changer_tx.send(Move{bay→slot})`, release the reservation, drop the mapping.
- `AppendObject`/`ReadObjectRange`/`ReadFile`/`Get`/`Close` look up `session_id →
  bay` and dispatch to that DriveActor; robotics (`MoveMedium`/`LoadDrive`/
  `UnloadDrive`/`RefreshInventory`) and `ReconcileTape` go to the ChangerActor (or
  changer-then-drive for reconcile).

## Behavior preservation (the 3a contract)

The global `reservation` `AtomicBool` admits **one session at a time**, exactly as
today's `session_busy`. So externally nothing changes: the same RPCs, the same
serialized semantics, the same `FAILED_PRECONDITION` when busy. The harness
scenarios E (write) / F (spine) / G (library) **must stay green unchanged** — that
is how we verify the structural rewrite is a behavioral no-op. The internal control
flow changes completely (one nested loop → multi-actor + async orchestration), but
the observable behavior does not. Concurrency is **not** turned on here.

## Catalog under the pool

WAL is already enabled. Each actor opens its own `CatalogIndex` connection
in-thread (the read path already opens `open_read_only` per call). Add a
`busy_timeout` PRAGMA on open so a writer that meets another writer waits briefly
instead of erroring `SQLITE_BUSY` (matters in 3b; harmless in 3a's serial mode).
Shared cross-actor state stays `Send+Sync`: the operations registry (`Arc`), the
audit append lock (`Arc<Mutex>`), the library snapshot cell (`Arc<RwLock>`).

## Scope

**IN:** `ChangerCommand` + `DriveCommand` enums; the ChangerActor + DriveActor
loops (the per-session loop moves onto the drive actor); `with_drive_pool` startup
(open drives → spawn DriveActors with the moved `DriveHandle`s; `into_changer` →
ChangerActor); the `DrivePool` registry in `ApiState`; the async `mount` module
(open/close/abort + the `session_id → bay` map); rewiring the ~21 handler dispatch
sites to the pool/mount; the **global** reservation (one session at a time);
`busy_timeout` on `CatalogIndex::open`.

**OUT (→ 3b):** per-drive reservation (the global flag stays in 3a); actually
running two sessions at once; the harness scenario for parallel writes. **OUT
(→ 3c):** `DRIVE_STATUS_BUSY` / live per-drive state in `GetLibrary`. **OUT (→ Phase
4):** mid-stream cancellation; supervisor restart / library-path-loss handling. No
behavior change in 3a.

## Acceptance criteria

1. **Behavior-preserving regression:** the full `cargo test --workspace` stays
   green; the daemon `serve_catalog` e2e passes; and (human-run, on akash) harness
   scenarios **E / F / G still pass unchanged** — the structural proof.
2. **Structure:** `ApiState` holds a `DrivePool` (no more single `session_tx`);
   `with_drive_pool` spawns 1 changer + N drive actors (N = installed drive bays);
   a write/read/reconcile session routes to a DriveActor via `session_id → bay`;
   robotics route to the ChangerActor; only one session admitted at a time
   (global reservation).
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-api` + `cargo clippy -p remanence-api
--all-targets -- -D warnings` (both clean) on 2026-06-04 with a skeleton
(`crates/remanence-api/src/p3_skeleton.rs`, since removed — design-only) using the
**real** `ChangerHandle`/`DriveHandle`/`CatalogIndex` types: a `DriveHandle` and a
`ChangerHandle` each move into a spawned `std::thread`; each actor opens its own
`CatalogIndex` in-thread; the `DrivePool` registry is `Send + Sync + Clone`; the
startup `for bay { library.open_drive(bay) } ; library.into_changer()` sequence
borrow-checks; and the async `mount` dispatches `ChangerCommand`/`DriveCommand`
over `mpsc` + awaits `oneshot` replies.

Five-category result:
1. **Module privacy** — pool/commands/mount live in `remanence-api` beside
   `ApiState`; use `pub` `ChangerHandle`/`DriveHandle` + `open_drive`/`into_changer`.
   Pass.
2. **`!Send` in threading** — the crux: `DriveHandle`/`ChangerHandle` are `Send`
   (Phase 2) so they move into their actor threads; `CatalogIndex` (`!Send`) is
   built **in** each thread, never moved; the `DrivePool` (Senders/`Arc`s) is
   `Send+Sync+Clone`. Verified.
3. **Reactor timing** — the async `mount` runs on the tonic runtime; the actors
   are plain `std::thread`s using `blocking_recv` (as the current owner does); no
   tokio type constructed off-runtime. Pass.
4. **Borrowed-handle plumbing** — startup opens drives via sequential `&mut
   library` borrows (each released) then `into_changer()` consumes; actors own
   their handles outright (no cross-actor borrows). Pass.
5. **Trait/method visibility** — `LibraryHandle::{open_drive, into_changer}`,
   `tokio::sync::{mpsc, oneshot}`, `CatalogIndex::open`, `Arc`/`Mutex`/`AtomicBool`
   reachable. Pass.

No new dependencies (`busy_timeout` is a PRAGMA; tokio/std already used).
