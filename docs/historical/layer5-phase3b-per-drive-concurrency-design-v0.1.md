# Layer 5 — Phase 3b-A: per-drive reservation → concurrent sessions Design v0.1

Status: design decision. Phase 3, sub-slice 3b (variant A) of the multi-drive
architecture (`docs/multidrive-concurrency-architecture-v0.1.md`). Builds on 3a
(actor pool, still serial). Goal: **flip the single global reservation to
per-drive so two write/read sessions stream in parallel**, while keeping
reconcile + robotics **pool-exclusive** (they keep 3a's fresh-handle approach,
gated so no session is active). This is where concurrency turns on.

## Background — 3a's single gate

3a's `DrivePool.reservation: Arc<AtomicBool>` is one global flag CAS'd by
**everything**: `mount::open_write/open_read` (`try_reserve`), and — via
`SessionBusyGuard::from_reserved(cfg.reservation)` on the changer actor —
`Reconcile` and `Robotics`. `handle_reconcile`/`handle_robotics` open **fresh**
`LibraryHandle`s + their own `DriveHandle`s and depend on that flag for
exclusivity. So sessions are serial, and reconcile/robotics are safe only because
nothing else runs.

## Design — per-bay flags + a pool-exclusive gate

Replace the single flag with **one flag per drive bay**:
```rust
struct DrivePool {
    changer_tx: mpsc::Sender<ChangerCommand>,
    drives: Arc<HashMap<u16, mpsc::Sender<DriveCommand>>>,
    reservations: Arc<HashMap<u16, AtomicBool>>,   // was: reservation: Arc<AtomicBool>
    sessions: Arc<Mutex<HashMap<Uuid, MountedSession>>>,
}
```
Methods (CAS on the per-bay flags):
- `reserve_free_drive() -> Result<u16, Status>` — pick a free bay (sorted bays →
  `into_iter().find(|&bay| cas(bay))` so it's deterministic *and* clippy-clean);
  all busy → `failed_precondition("all drives are busy")`. For **sessions**.
- `release(bay)`.
- `reserve_all_exclusive() -> Result<(), Status>` — CAS **all** bays free→true,
  rolling back the ones acquired if any is busy (`failed_precondition("drives are
  busy")`). For **reconcile/robotics**.
- `release_all()`; and an `ExclusiveGuard { reservations }` whose `Drop` releases
  all bays (the changer actor's fire-and-track ops hold it until completion,
  mirroring 3a's `SessionBusyGuard::from_reserved`).

`WriteOwnerConfig.reservation: Arc<AtomicBool>` → `reservations: Arc<HashMap<u16,
AtomicBool>>` (so the changer actor's `Reconcile`/`Robotics` arms build the
`ExclusiveGuard`). `with_drive_pool` builds the map with one `AtomicBool::new(false)`
per opened bay.

### Sessions (the concurrency win)

`mount::open_write`/`open_read` call `reserve_free_drive()` (instead of picking the
lone bay). Two `OpenWriteSession` calls reserve **different** bays → their MOVEs
queue at the (single) changer actor (one robot, serialized) and then **stream in
parallel** on their two drive actors. `MountedSession` gains `tape_uuid`; the in-use
set (mounted tapes, gathered from `sessions`) is passed as `select_tape_in_pool`'s
`reserved_tape_uuids` arg so two writes never select the same tape, and an
`open_read` for a tape already mounted elsewhere → `failed_precondition("tape is
already mounted")` (a physical tape can be in only one drive). `mount::close`/`abort`
`release(bay)` and drop the session.

### Reconcile + robotics (pool-exclusive, unchanged bodies)

The `Catalog.ReconcileTape` + `LibraryService` robotics handlers call
`reserve_all_exclusive()` (instead of `try_reserve`): `failed_precondition` if any
drive is busy. On success the op dispatches to the changer actor, which holds an
`ExclusiveGuard` until `handle_reconcile`/`handle_robotics` completes (releasing
all bays). Those bodies are **unchanged** — opening fresh handles is safe because
exclusive admission guarantees no session is touching the hardware. (Concurrent
reconcile-while-writing is the deferred 3b-B rework.)

## Behavior change (intended)

This is the first slice that changes observable behavior: **N drives stream
concurrently** (bounded by the shared SAS path, per the architecture doc). One
robot still serializes MOVEs; the catalog's per-connection WAL + `busy_timeout`
(3a) absorb concurrent index writes. Reconcile/robotics now return
`failed_precondition` when *any* drive is busy (previously when *the* session was
busy) — the pool-exclusive semantics.

## Scope

**IN:** per-bay `reservations` map + `reserve_free_drive`/`release`/
`reserve_all_exclusive`/`release_all` + `ExclusiveGuard`; `WriteOwnerConfig` +
`with_drive_pool` wiring; `mount` reserving a free bay + excluding in-use tapes +
the open_read already-mounted check + per-bay release on close/abort/error; the
reconcile/robotics handlers using `reserve_all_exclusive`; the changer arms using
`ExclusiveGuard`. **OUT:** routing reconcile/robotics through the drive actors for
concurrent-reconcile-while-writing (3b-B / later); `DRIVE_STATUS_BUSY` live state
(3c); mid-stream cancellation + supervisor restart (Phase 4).

## Acceptance criteria

1. **Unit:** `reserve_free_drive` returns distinct bays on repeated calls and
   `failed_precondition` when all are reserved; `release` frees one; after
   `reserve_all_exclusive`, `reserve_free_drive` fails, and a partial exclusive
   (one bay pre-reserved) rolls back and fails without leaving any bay reserved.
2. **Integration (hardware-free, multi-bay fixture pool):** two `mount::open_write`
   reserve different bays and both succeed; a third → `failed_precondition`; two
   concurrent writes select different tapes (in-use exclusion); `open_read` for a
   tape already mounted → `failed_precondition`; `ReconcileTape`/a robotics op
   while a session is open → `failed_precondition`; after closing the sessions, a
   reconcile succeeds.
3. **Harness e2e (akash, human-run, OUT of Codex scope):** two `OpenWriteSession`
   to the LTO-9 pool stream to two drives **simultaneously** (a new parallel-write
   scenario); a reconcile while a write is active is refused; closing frees the
   drive. (Also: the 3a E/F/G no-op proof still holds.)
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-api` (clean) on 2026-06-04 with a
skeleton (`crates/remanence-api/src/p3b_skeleton.rs`, since removed — design-only):
the per-bay `reservations: Arc<HashMap<u16, AtomicBool>>` pool stays
`Send + Sync + Clone`; `reserve_free_drive` (CAS-scan), `reserve_all_exclusive`
(all-or-rollback), and the `ExclusiveGuard` `Drop` all compile. Clippy flagged the
CAS-scan as `manual_implementation of Iterator::find` — the real impl uses
`bays.into_iter().find(|&bay| …cas…)` to be clippy-clean (noted in the plan).

Five-category result:
1. **Module privacy** — all in `remanence-api` (`write_owner`/`mount`/`lib`);
   `MountedSession`/`DrivePool` are `pub(crate)`. Pass.
2. **`!Send` in threading** — `Arc<HashMap<u16, AtomicBool>>` is `Send+Sync`; the
   pool stays `Send+Sync+Clone`; the `ExclusiveGuard` is `Send`. Pass.
3. **Reactor timing** — none (no new tokio constructs; CAS is sync). Pass.
4. **Borrowed-handle plumbing** — no new borrows; reservations are `Arc`-shared
   atomics. Pass.
5. **Trait/method visibility** — `AtomicBool`/`compare_exchange`, `HashMap`,
   `select_tape_in_pool`'s `reserved_tape_uuids: &HashSet<TapeUuid>` arg reachable.
   Pass.

No new dependencies.
