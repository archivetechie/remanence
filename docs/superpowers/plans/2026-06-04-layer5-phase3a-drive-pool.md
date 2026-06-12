# Layer 5 Phase 3a — drive-actor pool + mount orchestration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the single drive-session owner with a pool of actors — 1 ChangerActor (owns the `ChangerHandle`) + N DriveActors (each owns a `DriveHandle` + its own catalog connection) — coordinated by an async `mount` module, while a **global reservation keeps behavior unchanged** (one session at a time). 3b later flips the reservation to per-drive to turn on concurrency.

**Architecture:** Each actor is a `std::thread` with its own `mpsc` channel, building its own `!Send` `CatalogIndex` in-thread. `DriveHandle`/`ChangerHandle` are `Send` (Phase 2) so they move into their actor threads. `ApiState` holds a `DrivePool { changer_tx, drives, reservation, sessions }`. The async `mount` module sequences changer-MOVE → drive-session.

**Tech Stack:** Rust, `tokio::sync::{mpsc, oneshot}`, `std::thread`, `remanence-library` (`ChangerHandle`/`DriveHandle`), `remanence-state` (`CatalogIndex`, WAL already on).

---

## Context the implementer needs

- **Design doc:** `docs/layer5-phase3a-drive-pool-design-v0.1.md`; umbrella: `docs/multidrive-concurrency-architecture-v0.1.md`. **Behavior-preserving** (still serial via a global reservation) — the structural proof is that harness scenarios E/F/G stay green unchanged.
- **The verified skeleton** (`docs/…` §verification; the deleted `p3_skeleton.rs`) is the scaffold for the new types — `ChangerCommand`/`DriveCommand` enums, `spawn_changer_actor`/`spawn_drive_actor`, the `DrivePool` registry, `with_drive_pool` startup (`for bay { library.open_drive(bay,policy) }` → spawn DriveActors with the moved handles; `library.into_changer()` → ChangerActor), and the async `mount` (changer `Move` then drive `OpenWrite`, over `mpsc`+`oneshot`). It compiled clean with the real types.
- **Current owner** (`crates/remanence-api/src/write_owner.rs`): `spawn_session_owner` → `session_loop`; `handle_open` (write) + `handle_open_read` (read) each acquire `SessionBusyGuard::try_acquire(&cfg.session_busy)`, mount via `load_tape_by_uuid` (changer LOAD + `open_drive`), then run a **nested loop on the single `rx`** processing the session's commands until Close/Abort. `handle_reconcile` + `handle_robotics` are fire-and-track (S3a/S6b). `SessionCommand` enum + `WriteOwnerConfig`.
- **`ApiState`** (`lib.rs`): `session_tx: Option<mpsc::Sender<SessionCommand>>` + `session_busy: Option<Arc<AtomicBool>>`; `with_session_owner` builds them; ~21 handler/dispatch sites use `session_tx`/`reserve_session_owner`. `mount.rs::load_tape_by_uuid`. `main.rs` calls `with_session_owner`.
- **Key enabler:** `DriveHandle`/`ChangerHandle: Send` (Phase 2); the per-drive `DriveHandle` is opened from the `LibraryHandle` at startup (`open_drive`) and moved into its actor — the drive's `/dev/sg` fd persists across tape moves.

## File Structure

- **Modify** `crates/remanence-state/src/index.rs` — `busy_timeout` PRAGMA on open.
- **Rewrite** `crates/remanence-api/src/write_owner.rs` → the actor pool: `ChangerCommand`/`DriveCommand`, `spawn_changer_actor`/`spawn_drive_actor`, `with_drive_pool`, `DrivePool`. (Keep the per-session work — `write_to_selected_tape`, the read/reconcile bodies — reused inside the DriveActor loop.)
- **Create** `crates/remanence-api/src/mount.rs` additions (or a new `session_orchestration.rs`) — the async `mount::open_write`/`open_read`/`close`/`abort` + `session_id → bay`.
- **Modify** `crates/remanence-api/src/lib.rs` — `ApiState` holds `DrivePool` instead of `session_tx`/`session_busy`; rewire the ~21 dispatch sites through `mount`/pool.
- **Modify** `crates/remanence-api/src/main.rs` (daemon) — call `with_drive_pool`.

---

## Task 1: `busy_timeout` on the catalog (tiny prep)

**Files:** `crates/remanence-state/src/index.rs`

- [ ] **Step 1:** In the connection-open path (next to the existing `journal_mode=WAL` pragma at ~:3256), add:
```rust
    conn.pragma_update(None, "busy_timeout", 5000)
        .map_err(|err| sqlite_error("set sqlite busy_timeout", err))?;
```
(5 s; a second writer waits instead of erroring `SQLITE_BUSY` — needed once 3b runs writers concurrently, harmless serially.)
- [ ] **Step 2:** `cargo test -p remanence-state` (expect PASS).
- [ ] **Step 3:** Commit: `git commit -am "Layer5 P3a: busy_timeout on the catalog for concurrent connections"`.

---

## Task 2: The actor pool (commands + actors + startup)

**Files:** `crates/remanence-api/src/write_owner.rs` (rename concept to "drive pool"; keep the filename or rename to `drive_pool.rs` + update `mod`), `crates/remanence-api/src/lib.rs` (mod decl)

- [ ] **Step 1: Define the command enums**

`ChangerCommand` (to the ChangerActor): `Move { src: u16, dst: u16, reply: oneshot::Sender<Result<(), Status>> }`, `Refresh { reply }`, and the robotics + reconcile producers currently in `SessionCommand` (`Robotics { action, handle }`, `Reconcile { tape_uuid, handle }`) — these already run on the changer/owner today. `DriveCommand` (to a DriveActor): the per-session set — `OpenWrite { pool_id, selected, tape_uuid, reply }`, `AppendFinish {…}`, `Close {…}`, `Abort {…}`, `Get {…}`, `OpenRead { tape_uuid, reply }`, `ReadFile {…}`, `CloseRead {…}`, `GetRead {…}`. (Carry over the exact payload fields from today's `SessionCommand` variants.)

- [ ] **Step 2: The ChangerActor**

`fn spawn_changer_actor(changer: ChangerHandle, cfg: ChangerConfig) -> mpsc::Sender<ChangerCommand>` — a `std::thread` (`name "rem-changer"`) owning the `ChangerHandle` + its own `CatalogIndex` connection (opened in-thread from the config's index path) + the audit/operations deps; `blocking_recv` loop. `Move` → `changer.move_medium(src, dst, &policy)` (reply with mapped `Status`). `Refresh` → `changer.refresh()` then publish the snapshot cell (as S6b's owner does). `Robotics`/`Reconcile` → the existing `handle_robotics`/`handle_reconcile` bodies (they already use the changer + the operations registry; for reconcile, the changer mounts then does the scan — keep today's logic, now on this thread).

- [ ] **Step 3: The DriveActor**

`fn spawn_drive_actor(bay: u16, drive: DriveHandle, cfg: DriveConfig) -> mpsc::Sender<DriveCommand>` — a `std::thread` (`name format!("rem-drive-{bay}")`) owning the `DriveHandle` + its own `CatalogIndex` connection; `blocking_recv` loop holding per-session state (`Option<ActiveWrite>` / `Option<ActiveRead>`). Move the bodies from today's `handle_open` in-session loop (write: `OpenWrite` sets up `pool_cfg`/`selected`/counters; `AppendFinish` → `write_to_selected_tape(index, &mut DriveHandleSink(&mut drive), …)`; `Close`/`Abort`/`Get` → `session_proto`) and `handle_open_read` (read: `OpenRead` mounts/pins; `ReadFile` streams via the read core; `CloseRead`/`GetRead`) — now keyed to **this** drive's channel + handle rather than the global `rx`. The mount MOVE is **not** done here (the ChangerActor does it before `OpenWrite` arrives); `OpenWrite`/`OpenRead` assume the tape is already in this bay and do the drive-side work (SSC LOAD via the held `DriveHandle`, verify identity, then I/O).

- [ ] **Step 4: `with_drive_pool` (startup)**

```rust
pub fn with_drive_pool(index: CatalogIndex, config: &RemConfig, report: DiscoveryReport,
                       policy: StaticAllowlist, spool_dir: PathBuf) -> ApiState
```
mirrors `with_session_owner` but: open the `LibraryHandle` (`report.library(serial)?.open(&policy)?`); for each drive bay with an installed drive + `sg_path`, `library.open_drive(bay, &policy)?` and `drives.insert(bay, spawn_drive_actor(bay, drive, drive_cfg))`; then `spawn_changer_actor(library.into_changer(), changer_cfg)`; build the `DrivePool { changer_tx, drives: Arc::new(drives), reservation: Arc::new(AtomicBool::new(false)), sessions: Arc::new(Mutex::new(HashMap::new())) }`; store it in `ApiState`. Each actor cfg carries the index path (each opens its own connection), the policy, pool configs, audit dir/fsync/lock, the operations registry, and the library snapshot cell.

- [ ] **Step 5:** `cargo check -p remanence-api`. Resolve compile errors. (The handlers in `lib.rs` still reference the old `session_tx` — they're rewired in Task 4; until then, gate this task's check on the pool module compiling in isolation, or land Tasks 2-4 together before the workspace builds.) Commit when `-p remanence-api` type-checks the pool module.

---

## Task 3: `ApiState` holds the `DrivePool`

**Files:** `crates/remanence-api/src/lib.rs`, `crates/remanence-api/src/main.rs` (daemon)

- [ ] **Step 1:** Replace `ApiState`'s `session_tx: Option<mpsc::Sender<SessionCommand>>` + `session_busy: Option<Arc<AtomicBool>>` with `drive_pool: Option<DrivePool>`. Default `None` in the inner ctor. `with_drive_pool` sets `Some(pool)`.
- [ ] **Step 2:** Replace `reserve_session_owner` with `DrivePool`-based reservation helpers: `try_reserve(&self) -> Result<ReservationGuard, Status>` (CAS the global `reservation` flag, `failed_precondition` if busy — same semantics as today), and `record session_id → bay` / `lookup bay for session` / `release` on `self.sessions`.
- [ ] **Step 3:** `main.rs`: call `with_drive_pool(...)` instead of `with_session_owner(...)` on the write-capable path (read-only path unchanged — no pool).
- [ ] **Step 4:** `cargo check -p remanence-api` (with Task 4's handler rewiring, the workspace builds).

---

## Task 4: Async `mount` module + rewire the handlers

**Files:** `crates/remanence-api/src/mount.rs` (add the orchestration) + `crates/remanence-api/src/lib.rs`

- [ ] **Step 1: `mount::open_write`**

```rust
pub(crate) async fn open_write(state: &ApiState, pool_id: &str, library_uuid: &[u8])
    -> Result<pb::WriteSession, Status>
```
acquire the global reservation; select the tape (`select_tape_in_pool` over `state.index()?` — a read-only connection); choose the single free drive bay (in 3a, the lone bay not currently in a session — with one global reservation there is always exactly one session, so pick any installed bay deterministically, e.g. lowest element address); `changer_tx.send(ChangerCommand::Move { src: slot, dst: bay, reply })` + await; `drive_tx(bay).send(DriveCommand::OpenWrite { … })` + await; on success record `sessions[session_id] = bay` and return the `WriteSession`. On any step's error, release the reservation (and, if the MOVE already happened, send the compensating `Move { bay→slot }`). `open_read` is the analogous read path; `close`/`abort` route to the session's DriveActor to finalize, then `changer Move { bay→slot }`, release, and drop the mapping.

- [ ] **Step 2: Rewire the handlers** (the ~21 sites in `lib.rs`)

Mechanical, by category:
- `open_write_session` → `mount::open_write(&self.state, …).await`; `open_read_session` → `mount::open_read`.
- `append_object`/`close_write_session`/`abort_write_session`/`get_write_session` and `read_object_range`/`read_file`/`close_read_session`/`get_read_session`: look up the bay via `self.state.bay_for_session(session_id)?` then dispatch the corresponding `DriveCommand` to `self.state.drive_pool…drives[bay]` (replacing today's `self.state.session_tx.send(SessionCommand::…)`). Missing session → `not_found`.
- `Catalog.reconcile_tape` and the `LibraryService` robotics handlers (`refresh_inventory`/`move_medium`/`load_drive`/`unload_drive`): dispatch to `changer_tx` (`ChangerCommand::Reconcile`/`Robotics`) instead of the old `session_tx`. The reservation + `OperationRef` flow is unchanged (S3a/S6b), now against the changer actor.
- Replace `reserve_session_owner()` calls with `try_reserve()`; "no session owner (read-only mode)" `unavailable` errors map to "no drive pool" when `drive_pool` is `None`.

- [ ] **Step 3:** `cargo build --workspace` (expect clean — the whole workspace compiles for the first time since Task 2).
- [ ] **Step 4:** Commit: `git commit -am "Layer5 P3a: drive-actor pool + async mount; ApiState holds DrivePool"`.

---

## Task 5: Gates + behavior-preserving verification

- [ ] **Step 1:** `cargo fmt --all`.
- [ ] **Step 2:** `cargo clippy --workspace --all-targets -- -D warnings` (expect clean).
- [ ] **Step 3:** `cargo test --workspace` (expect PASS — the full regression incl. the daemon `serve_catalog` e2e; the existing in-crate write/read/reconcile tests, now exercising the pool, stay green).
- [ ] **Step 4:** Commit: `git commit -am "Layer5 P3a: fmt + gates"`.
- [ ] **Step 5 (human-run, OUT of Codex scope — note in the journal):** on the akash fixture, harness scenarios **E (write) / F (spine) / G (library)** must pass **unchanged** — the behavioral no-op proof that the structural rewrite is sound. (No new parallel-write scenario yet; that lands with 3b.)

---

## Self-Review (completed during planning)

**Spec coverage:** `busy_timeout` (Task 1) ✓; `ChangerCommand`/`DriveCommand` + the two actors + per-drive in-session loop (Task 2.1-3) ✓; `with_drive_pool` startup open-drives-then-`into_changer` (Task 2.4) ✓; `DrivePool` in `ApiState` + global reservation + `session_id→bay` (Task 3) ✓; async `mount` open/close/abort + handler rewiring of all dispatch sites (Task 4) ✓; behavior-preserving regression incl. harness E/F/G (Task 5) ✓. OUT items (per-drive reservation/parallelism → 3b; `DRIVE_STATUS_BUSY` → 3c; cancellation/supervision → Phase 4) untouched.

**Placeholder scan:** the bodies "moved from `handle_open`/`handle_open_read`" (Task 2.3) and "the existing `handle_robotics`/`handle_reconcile`" (Task 2.2) reference concrete existing code being relocated (a move, not new logic — reproducing 130+ lines verbatim adds no value); the handler rewiring (Task 4.2) is a precise per-category transformation against the enumerated call sites. The novel structural code (enums, actors, `with_drive_pool`, `DrivePool`, async `mount`) is given by the verified skeleton + Task steps. Build checkpoints are flagged where the workspace is transiently broken (Tasks 2-4 land together for a green `cargo build --workspace`).

**Type consistency:** `DrivePool { changer_tx: mpsc::Sender<ChangerCommand>, drives: Arc<HashMap<u16, mpsc::Sender<DriveCommand>>>, reservation: Arc<AtomicBool>, sessions: Arc<Mutex<HashMap<Uuid,u16>>> }` (verified `Send+Sync+Clone`); `DriveHandle`/`ChangerHandle` move into their threads (`Send`, Phase 2); each actor opens its own `CatalogIndex` (`!Send`) in-thread; `session_id → bay` routes session RPCs; the global `reservation` preserves today's one-session-at-a-time semantics.
