# Layer 5 Phase 3c — live `DRIVE_STATUS_BUSY` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Report a drive currently in a session as `DRIVE_STATUS_BUSY` in `GetLibrary`, using 3b-A's per-bay reservation flags.

**Architecture:** `DrivePool::busy_bays()` reads the reservation flags; `get_library` threads the busy set into the drive-status projection (BUSY overrides LOADED/IDLE; UNREACHABLE still wins).

**Tech Stack:** Rust, `remanence-api` (`write_owner`/`lib`/`library`).

---

## Context the implementer needs

- **Design:** `docs/layer5-phase3c-drive-status-busy-design-v0.1.md`. Small finisher to Phase 3.
- **Current** (`crates/remanence-api/src/library.rs`): `drive_status(bay: &DriveBay) -> pb::drive::Status` (UNREACHABLE/LOADED/IDLE from the snapshot); `project_drive(bay, voltags: &HashMap<String, Vec<u8>>) -> pb::Drive` (`status: drive_status(bay) as i32`); `project_library_state(library, captured_at, voltags)`; `get_library` builds `voltags` then calls `project_library_state`. `pb::drive::Status::DriveStatusBusy` exists (generated).
- **3b-A pool** (`write_owner.rs`): `DrivePool.reservations: Arc<HashMap<u16, AtomicBool>>` (true while a bay is reserved). `ApiState.drive_pool: Option<DrivePool>` (`lib.rs`).

## File Structure

- **Modify** `crates/remanence-api/src/write_owner.rs` — `DrivePool::busy_bays`.
- **Modify** `crates/remanence-api/src/lib.rs` — `ApiState::busy_drive_bays`.
- **Modify** `crates/remanence-api/src/library.rs` — `busy` param on `drive_status`; thread `busy_bays` through `project_drive`/`project_library_state`/`get_library`; tests.

---

## Task 1: Pool + ApiState accessors

- [ ] **Step 1:** `write_owner.rs` `impl DrivePool`:
```rust
    pub(crate) fn busy_bays(&self) -> std::collections::HashSet<u16> {
        self.reservations
            .iter()
            .filter(|(_, flag)| flag.load(std::sync::atomic::Ordering::SeqCst))
            .map(|(bay, _)| *bay)
            .collect()
    }
```
- [ ] **Step 2:** `lib.rs` `impl ApiState`:
```rust
    pub(crate) fn busy_drive_bays(&self) -> std::collections::HashSet<u16> {
        self.drive_pool
            .as_ref()
            .map(|pool| pool.busy_bays())
            .unwrap_or_default()
    }
```
- [ ] **Step 3:** `cargo check -p remanence-api`.

---

## Task 2: Thread `busy_bays` into the projection

**Files:** `crates/remanence-api/src/library.rs`

- [ ] **Step 1:** Add the `busy` param + precedence to `drive_status`:
```rust
pub(crate) fn drive_status(bay: &DriveBay, busy: bool) -> pb::drive::Status {
    match bay.installed.as_ref() {
        None => pb::drive::Status::DriveStatusUnreachable,
        Some(installed) if installed.sg_path.is_none() => pb::drive::Status::DriveStatusUnreachable,
        Some(_) if busy => pb::drive::Status::DriveStatusBusy,
        Some(_) if bay.loaded => pb::drive::Status::DriveStatusLoaded,
        Some(_) => pb::drive::Status::DriveStatusIdle,
    }
}
```
- [ ] **Step 2:** `project_drive` takes `busy_bays: &HashSet<u16>` and computes `let busy = busy_bays.contains(&bay.element_address);` → `status: drive_status(bay, busy) as i32`. `project_library_state` takes `busy_bays: &HashSet<u16>` and forwards it to each `project_drive` call.
- [ ] **Step 3:** `get_library`: after building `voltags`, add `let busy_bays = self.state.busy_drive_bays();` and pass `&busy_bays` to `project_library_state`.
- [ ] **Step 4:** Update the existing S6a tests that call `drive_status(...)` / `project_library_state(...)` for the new param (pass `false` / an empty `&HashSet::new()` to preserve their assertions). Add:
  - a `drive_status` case asserting `drive_status(&installed_reachable_bay, true) == DriveStatusBusy` and that an unreachable bay stays `UNREACHABLE` with `busy = true`;
  - a `project_library_state` case with a `busy_bays` set containing the drive's `element_address` → `drives[0].status == DriveStatusBusy as i32`.
- [ ] **Step 5:** `cargo test -p remanence-api library::tests`.

---

## Task 3: Gates

- [ ] **Step 1:** `cargo fmt --all`.
- [ ] **Step 2:** `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] **Step 3:** `cargo test --workspace`.
- [ ] **Step 4:** Commit: `git commit -am "Layer5 P3c: live DRIVE_STATUS_BUSY in GetLibrary"`.

---

## Self-Review (completed during planning)

**Spec coverage:** `DrivePool::busy_bays` + `ApiState::busy_drive_bays` (Task 1) ✓; `busy` param + precedence on `drive_status`, threaded through `project_drive`/`project_library_state`/`get_library` (Task 2) ✓; tests incl. the BUSY case + the S6a regression (Task 2.4) ✓; gates (Task 3) ✓. OUT (busy-kind detail, snapshot-liveness, S6c, Phase 4) untouched.

**Placeholder scan:** every step has concrete code; the test updates name the exact existing call sites to fix (the S6a `drive_status`/`project_library_state` tests) plus the new BUSY assertions.

**Type consistency:** `busy_bays() -> HashSet<u16>` (keyed on `bay.element_address: u16`); `drive_status(bay, busy: bool)`; `project_drive`/`project_library_state` gain `&HashSet<u16>`; `get_library` sources it from `ApiState::busy_drive_bays()` (empty when no pool). `DriveStatusBusy` is the generated variant.
