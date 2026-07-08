# Layer 5 — Phase 3c: live `DRIVE_STATUS_BUSY` in `GetLibrary` Design v0.1

Status: design decision. Phase 3, sub-slice 3c (the small finisher) of the
multi-drive architecture (`docs/multidrive-concurrency-architecture-v0.1.md`).
Builds on 3b-A (per-bay reservation flags). Goal: surface a drive that is
currently in a session as **`DRIVE_STATUS_BUSY`** in `GetLibrary`, completing the
S6a/S6b drive-status story (S6a explicitly deferred BUSY for lack of live state).

## Background

S6a's `drive_status(&DriveBay)` derives `UNREACHABLE`/`LOADED`/`IDLE` from the
**static** inventory snapshot and noted BUSY as deferred ("the static snapshot
can't attribute the global busy flag to a specific bay"). 3b-A gave us exactly the
missing signal: `DrivePool.reservations: Arc<HashMap<u16, AtomicBool>>` — one flag
per bay, set true while a session (or pool-exclusive op) holds that drive.

## Design

- **Pool accessor:** `DrivePool::busy_bays(&self) -> HashSet<u16>` — the bays whose
  reservation flag reads `true`. `ApiState::busy_drive_bays(&self) -> HashSet<u16>`
  returns `self.drive_pool.as_ref().map(|p| p.busy_bays()).unwrap_or_default()`
  (empty on a read-only daemon with no pool).
- **Status precedence:** `drive_status(bay: &DriveBay, busy: bool)`:
  ```
  installed.is_none() | sg_path.is_none()  -> UNREACHABLE
  busy                                     -> BUSY      (live, from the reservation)
  bay.loaded                               -> LOADED    (snapshot-derived)
  else                                     -> IDLE      (snapshot-derived)
  ```
  BUSY overrides LOADED/IDLE; UNREACHABLE still wins (an unreachable drive can't be
  reserved). `BUSY` is the **live** signal; `LOADED`/`IDLE` remain snapshot-derived
  (stale until `RefreshInventory`) — unchanged from S6a/S6b.
- **Threading:** `project_drive(bay, voltags, busy_bays: &HashSet<u16>)` passes
  `busy_bays.contains(&bay.element_address)` to `drive_status`; `project_library_state`
  takes `busy_bays` and forwards it; `get_library` gathers it via
  `self.state.busy_drive_bays()` and passes it in. `ListLibraries` (identity only,
  no per-drive detail) is unchanged.

## Scope

**IN:** `DrivePool::busy_bays` + `ApiState::busy_drive_bays`; the `busy` param on
`drive_status` + the precedence; threading `busy_bays` through
`project_drive`/`project_library_state`/`get_library`; tests.

**OUT:** distinguishing *what kind* of busy (write vs read vs reconcile) — BUSY is
binary; `WatchOperation`/`ListOperations` already give op detail. Refreshing the
`LOADED`/`IDLE` snapshot liveness (that's `RefreshInventory`, S6b). `StreamLibraryEvents`
(S6c). Phase 4 (cancellation/supervision).

## Acceptance criteria

1. **Unit:** `drive_status(bay, true)` → `BUSY` for an installed+reachable bay;
   `drive_status(bay, false)` keeps the S6a LOADED/IDLE/UNREACHABLE results; an
   unreachable bay stays `UNREACHABLE` even when `busy` is true.
2. **Integration (hardware-free):** `get_library` with a snapshot + an `ApiState`
   whose pool reports bay X busy → that drive's `status == DRIVE_STATUS_BUSY`; with
   no busy bays (or no pool) → the S6a statuses (regression).
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

**Light verification (no skeleton), per the skill's "skip if mechanical / no new
types" guidance.** 3c adds no new structs/traits/lifetimes/async — only a
`busy: bool` parameter on a pure function, a `HashSet<u16>` threaded through two
projection fns, and a `DrivePool` accessor reading existing `AtomicBool`s into a
set. No Send/`!Send`, reactor-timing, borrow-plumbing, or module-privacy concerns
arise (all items already live in `remanence-api`; `HashSet`/`AtomicBool::load` are
in scope). The implementation's `cargo check` + the existing S6a drive-status unit
tests (updated for the new param) are the verification.
