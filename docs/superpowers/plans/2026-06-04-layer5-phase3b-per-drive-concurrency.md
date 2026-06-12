# Layer 5 Phase 3b-A — per-drive reservation (concurrency ON) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Flip the single global `DrivePool` reservation to one flag per drive bay, so two write/read sessions reserve different drives and stream in parallel; keep reconcile/robotics pool-exclusive (gated so their fresh-handle bodies stay safe).

**Architecture:** `DrivePool.reservation: Arc<AtomicBool>` → `reservations: Arc<HashMap<u16, AtomicBool>>`. Sessions `reserve_free_drive()`; reconcile/robotics `reserve_all_exclusive()`. The changer still serializes MOVEs; drive actors stream concurrently.

**Tech Stack:** Rust, `std::sync::atomic`, `remanence-api` (`write_owner`/`mount`/`lib`).

---

## Context the implementer needs

- **Design:** `docs/layer5-phase3b-per-drive-concurrency-design-v0.1.md`; umbrella: `docs/multidrive-concurrency-architecture-v0.1.md`. This is the slice that **turns concurrency on**.
- **Landed 3a** (`crates/remanence-api/src/write_owner.rs`): `DrivePool { changer_tx, drives: Arc<HashMap<u16, mpsc::Sender<DriveCommand>>>, reservation: Arc<AtomicBool>, sessions: Arc<Mutex<HashMap<Uuid, MountedSession>>> }` with `try_reserve`/`release_reservation`/`drive_tx(bay)`/`changer_tx`/`record_session`/`session(id)`. `MountedSession { bay, home_slot }`. `WriteOwnerConfig.reservation: Arc<AtomicBool>`. The changer loop's `ChangerCommand::{Reconcile,Robotics}` arms wrap each op in `SessionBusyGuard::from_reserved(cfg.reservation.clone())`. `with_drive_pool` (in `lib.rs`) builds the pool. `mount.rs` `open_write_session`/`open_read_session` call `pool.try_reserve()?`; `close_*`/`abort_*` call `pool.release_reservation()`. `select_tape_in_pool(index, pool_cfg, object_size, reserved_tape_uuids: &HashSet<TapeUuid>)` — the 4th arg excludes tapes (3a passes `&HashSet::new()`).
- **Clippy note:** write `reserve_free_drive` as `bays.into_iter().find(|&bay| …cas…)` (not a `for` loop) — clippy rejects the manual loop as `manual_implementation of Iterator::find`.

## File Structure

- **Modify** `crates/remanence-api/src/write_owner.rs` — `DrivePool` per-bay reservations + methods + `ExclusiveGuard`; `MountedSession.tape_uuid`; `WriteOwnerConfig.reservations`; the changer Reconcile/Robotics arms.
- **Modify** `crates/remanence-api/src/lib.rs` — `with_drive_pool` builds the reservations map; the `ReconcileTape` + robotics handlers use `reserve_all_exclusive`.
- **Modify** `crates/remanence-api/src/mount.rs` — `reserve_free_drive` + in-use-tape exclusion + open_read already-mounted check + per-bay release.

---

## Task 1: Per-bay reservations on `DrivePool`

**Files:** `crates/remanence-api/src/write_owner.rs`

- [ ] **Step 1: Field + constructor.** Change `DrivePool.reservation: Arc<AtomicBool>` → `reservations: Arc<HashMap<u16, AtomicBool>>`. Update `DrivePool::new` to take the map. Add `tape_uuid: TapeUuid` (or `[u8;16]`) to `MountedSession`.

- [ ] **Step 2: Methods** (replace `try_reserve`/`release_reservation`):
```rust
    pub(crate) fn reserve_free_drive(&self) -> Result<u16, Status> {
        let mut bays: Vec<u16> = self.reservations.keys().copied().collect();
        bays.sort_unstable();
        bays.into_iter()
            .find(|bay| {
                self.reservations[bay]
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            })
            .ok_or_else(|| Status::failed_precondition("all drives are busy"))
    }
    pub(crate) fn release(&self, bay: u16) {
        if let Some(flag) = self.reservations.get(&bay) {
            flag.store(false, Ordering::SeqCst);
        }
    }
    pub(crate) fn reserve_all_exclusive(&self) -> Result<(), Status> {
        let mut acquired = Vec::new();
        for (bay, flag) in self.reservations.iter() {
            if flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                acquired.push(*bay);
            } else {
                for got in acquired { self.reservations[&got].store(false, Ordering::SeqCst); }
                return Err(Status::failed_precondition("drives are busy"));
            }
        }
        Ok(())
    }
    pub(crate) fn release_all(&self) {
        for flag in self.reservations.values() { flag.store(false, Ordering::SeqCst); }
    }
    pub(crate) fn mounted_tape_uuids(&self) -> HashSet<TapeUuid> {
        self.sessions.lock().expect("sessions lock")
            .values().map(|s| s.tape_uuid).collect()
    }
    pub(crate) fn is_tape_mounted(&self, tape_uuid: &TapeUuid) -> bool {
        self.sessions.lock().expect("sessions lock").values().any(|s| &s.tape_uuid == tape_uuid)
    }
```

- [ ] **Step 3: `ExclusiveGuard`** (release-all-on-drop, for the changer's fire-and-track ops):
```rust
pub(crate) struct ExclusiveGuard { reservations: Arc<HashMap<u16, AtomicBool>> }
impl ExclusiveGuard {
    pub(crate) fn from_reserved(reservations: Arc<HashMap<u16, AtomicBool>>) -> Self { Self { reservations } }
}
impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        for flag in self.reservations.values() { flag.store(false, Ordering::SeqCst); }
    }
}
```

- [ ] **Step 4: `WriteOwnerConfig`** — change `reservation: Arc<AtomicBool>` → `reservations: Arc<HashMap<u16, AtomicBool>>`. In the changer loop, replace the `Reconcile`/`Robotics` arms' `let _busy_guard = SessionBusyGuard::from_reserved(cfg.reservation.clone());` with `let _exclusive = ExclusiveGuard::from_reserved(cfg.reservations.clone());`. (The `drain_failed_changer_commands` path that did `reservation.store(false)` now `release_all`s the map.) `handle_reconcile`/`handle_robotics` bodies are **unchanged**.

- [ ] **Step 5: Unit tests** (in `write_owner.rs` `mod tests`): a `DrivePool` over bays `{1,2}` — `reserve_free_drive` twice → `1` then `2`, third → `failed_precondition`; `release(1)` then reserve → `1`; `reserve_all_exclusive` fails when bay 1 is held and rolls back (bay 2 not left reserved); succeeds when all free; `ExclusiveGuard` drop frees all.

- [ ] **Step 6:** `cargo check -p remanence-api` (the `lib.rs`/`mount.rs` callers of the old methods will fail to compile until Tasks 2-3; land Tasks 1-3 together for a green `-p remanence-api`). Commit when the unit tests pass: `git commit -am "Layer5 P3b: per-bay DrivePool reservations + pool-exclusive gate"`.

---

## Task 2: `with_drive_pool` + reconcile/robotics handlers

**Files:** `crates/remanence-api/src/lib.rs`

- [ ] **Step 1:** In `with_drive_pool`, build `reservations`: after collecting `drive_txs`, create `let reservations: HashMap<u16, AtomicBool> = drive_txs.keys().map(|&bay| (bay, AtomicBool::new(false))).collect();` and pass `Arc::new(reservations)` into both `base_cfg` (replacing the old `reservation`) and `DrivePool::new`. (Build the map from the same bays that got drive actors.)
- [ ] **Step 2:** The `Catalog::reconcile_tape` handler and the `LibraryService` robotics handlers (`refresh_inventory`/`move_medium`/`load_drive`/`unload_drive`) currently `self.state.drive_pool()?.try_reserve()?` (or equivalent global reserve) before dispatching to the changer — replace with `…reserve_all_exclusive()?`. The fire-and-track dispatch + `OperationRef` flow is otherwise unchanged; the changer's `ExclusiveGuard` (Task 1 Step 4) releases all bays when the op completes. (RefreshInventory: if it is changer-RES-only with no fresh handles, it may keep a lighter gate — but for safety in 3b-A, gate it pool-exclusive too unless it demonstrably touches no drive.)
- [ ] **Step 3:** `cargo check -p remanence-api`.

---

## Task 3: `mount` — reserve a free drive + exclude in-use tapes

**Files:** `crates/remanence-api/src/mount.rs`

- [ ] **Step 1:** `open_write_session`: replace `pool.try_reserve()?` with `let bay = pool.reserve_free_drive()?;`. Thread `bay` into `open_write_session_reserved` so the mount targets that bay (the `ChangerCommand::Move{dst: bay}` + `drive_tx(bay)`); on the error path `pool.release(bay)` (not `release_reservation`).
- [ ] **Step 2:** Tape selection excludes in-use tapes: `let in_use = pool.mounted_tape_uuids(); let selected = select_tape_in_pool(&index, &pool_cfg, 0, &in_use)...;`. Record `MountedSession { bay, home_slot, tape_uuid: selected.tape_uuid }`.
- [ ] **Step 3:** `open_read_session`: `let bay = pool.reserve_free_drive()?;`; before mounting, `if pool.is_tape_mounted(&tape_uuid) { pool.release(bay); return Err(Status::failed_precondition("tape is already mounted")); }`; record `MountedSession { bay, home_slot, tape_uuid }`; release on error.
- [ ] **Step 4:** `close_*`/`abort_*`: replace `pool.release_reservation()` with `pool.release(mounted.bay)` (the bay from the session's `MountedSession`).
- [ ] **Step 5:** `cargo build --workspace` (first green build since Task 1). Commit: `git commit -am "Layer5 P3b: mount reserves a free drive + excludes in-use tapes; per-bay release"`.

---

## Task 4: Integration tests + gates

**Files:** `crates/remanence-api/src/mount.rs` or `lib.rs` `mod tests`

- [ ] **Step 1:** Add hardware-free integration tests against a `DrivePool` built over a multi-bay fixture (mirror the 3a `drive_pool_reservation_is_exclusive_until_released` setup): two `reserve_free_drive` give distinct bays, a third → `failed_precondition`; `reserve_all_exclusive` fails while a bay is reserved; the in-use-tape exclusion (a `MountedSession` recorded → `mounted_tape_uuids` contains it → `is_tape_mounted` true). (Full mount RPC paths need the actor threads + a catalog; keep these to the pool/reservation logic, which is the 3b-A surface.)
- [ ] **Step 2:** `cargo fmt --all`.
- [ ] **Step 3:** `cargo clippy --workspace --all-targets -- -D warnings` (expect clean — `reserve_free_drive` uses `.find()`).
- [ ] **Step 4:** `cargo test --workspace` (expect PASS — full regression; the 3a serial behavior still holds for single sessions; the new reservation tests pass).
- [ ] **Step 5:** Commit: `git commit -am "Layer5 P3b: concurrency tests + gates"`.
- [ ] **Step 6 (human-run, OUT of Codex scope — note in journal):** on akash, two `OpenWriteSession` to the LTO-9 pool stream to two drives **simultaneously** (new parallel-write harness scenario); a reconcile while a write is active → `FAILED_PRECONDITION`; the 3a E/F/G scenarios still pass.

---

## Self-Review (completed during planning)

**Spec coverage:** per-bay `reservations` + `reserve_free_drive`/`release`/`reserve_all_exclusive`/`release_all`/`ExclusiveGuard` (Task 1) ✓; `with_drive_pool` map + reconcile/robotics `reserve_all_exclusive` (Task 2) ✓; `mount` free-drive reservation + in-use-tape exclusion + open_read already-mounted check + per-bay release + `MountedSession.tape_uuid` (Task 3) ✓; tests + gates (Task 4) ✓. OUT (concurrent-reconcile rework / `DRIVE_STATUS_BUSY` / cancellation+supervision) untouched.

**Placeholder scan:** the reservation methods are given in full; `reserve_free_drive` uses `.find()` (clippy-clean, per the verification note); the handler edits (Task 2.2) and mount edits (Task 3) are precise per-site swaps against the named 3a call sites; build checkpoints flag that Tasks 1-3 land together for a green workspace.

**Type consistency:** `reservations: Arc<HashMap<u16, AtomicBool>>` is identical on `DrivePool` and `WriteOwnerConfig`; `MountedSession { bay, home_slot, tape_uuid }`; `reserve_free_drive() -> Result<u16, Status>` returns the bay threaded into mount; `select_tape_in_pool(..., &in_use)` matches the existing `reserved_tape_uuids: &HashSet<TapeUuid>` signature; `ExclusiveGuard::from_reserved` mirrors 3a's `SessionBusyGuard::from_reserved` lifecycle.
