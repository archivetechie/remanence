# Layer 2 Phase 1 — own the `DriveHandle` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drop `DriveHandle`'s `'a` lifetime so two drives can be open per library, by sharing `LibraryHandle`'s `audit_hook` + `dirty` through `Arc<Mutex<DriveShared>>`. Behavior-neutral; no concurrency turned on yet.

**Architecture:** `LibraryHandle` and each `DriveHandle` hold an `Arc<Mutex<DriveShared>>` clone (the audit hook + dirty state). `open_drive` clones the Arc instead of `&'a mut`-borrowing, so the returned handle is owned. Every audit-fire / dirty-mark becomes a brief lock (never held across a CDB). The lifetime removal ripples mechanically through the wrapper types and signatures in 4 crates.

**Tech Stack:** Rust, `std::sync::{Arc, Mutex}`, `remanence-library` Layer 2.

---

## Context the implementer needs

- **Design doc:** `docs/layer2-phase1-owned-drivehandle-design-v0.1.md`; umbrella: `docs/multidrive-concurrency-architecture-v0.1.md`. Read the behavior-neutrality argument first.
- **The only coupling** (`crates/remanence-library/src/handle/mod.rs`): `DriveHandle<'a>` (struct at :1236) holds `audit_hook: &'a mut Option<AuditHook>` and `dirty: &'a mut DirtyState`, created by `open_drive` (:677, construction at :756-763). `AuditHook = Box<dyn FnMut(&AuditEvent<'_>) + Send>` (:63). `DirtyState` (:120, `pub(crate)`, methods `mark`/`clear`/`is_dirty`/`cause` at :142-161) is unchanged — it just moves inside `DriveShared`.
- **Audit/dirty access sites** in `handle/mod.rs` (all become brief locks): the `fire_audit`/`fire_refused`/`fire_warnings` call sites in `move_medium_as`/`load`/`unload`/`export`/`import`/`refresh`/`rescan` (≈ :308, 325, 366, 391, 420, 442, 444, 468, 538, 554, and the refresh/rescan bodies); `set_audit_hook` (:226-230); `LibraryHandle::is_dirty` (:239) / cause; the `DriveHandle` SSC path `issue_load_unload` + `mark_dirty` (≈ :1355, :1385) and `DriveHandle::is_dirty`/`dirty_cause`. The helpers `fire_audit`(:1407)/`fire_refused`(:1482)/`fire_warnings` keep their `&mut Option<AuditHook>` signatures.
- **Key safety invariant:** the CDB execution (`self.transport.execute_*`) in `move_medium_as`/etc. sits *between* separate `fire_audit` statements — so per-site `lock().unwrap()` (a temporary guard that drops at the end of each fire/mark statement) is **never** held across a CDB. Do not introduce a guard that spans an `execute_*`.
- **Lifetime ripple sites** (every `DriveHandle<'…>`): `block_io.rs:101,118` (`DriveHandleSink`/`Source`), `physical_io.rs:92` (`DriveHandlePhysicalSource`), `remanence-parity/src/raw.rs:292,304` (`DriveHandleRawSource`/`RawSink`), `mount.rs:88-93` (`load_tape_by_uuid`), `write_owner.rs:1281,1363`, `remanence-cli/src/lib.rs:3771,4137`, plus `handle/mod.rs` impls (:1259 Debug, :1278, `handle/tape_io/mod.rs:255`).
- **Daemon does not use audit_hook/dirty** (no `set_audit_hook`, no `is_dirty` read in `write_owner.rs`) — so the share is invisible to it; only CLI/tests exercise the shared semantics, which `Arc<Mutex>` preserves.

## File Structure

- **Modify** `crates/remanence-library/src/handle/mod.rs` — `DriveShared`; `LibraryHandle.shared`; `DriveHandle` loses `'a`, holds `shared`; `open_drive` clones; lock all audit/dirty accesses; impl-lifetime removals.
- **Modify** `crates/remanence-library/src/{block_io.rs, physical_io.rs}` and `crates/remanence-library/src/handle/tape_io/mod.rs` — wrapper/impl lifetime arity.
- **Modify** `crates/remanence-parity/src/raw.rs` — `DriveHandleRawSource`/`RawSink` lifetime arity.
- **Modify** `crates/remanence-api/src/{mount.rs, write_owner.rs}` and `crates/remanence-cli/src/lib.rs` — signature ripples.

---

## Task 1: Core type change in `remanence-library`

**Files:** `crates/remanence-library/src/handle/mod.rs`, `block_io.rs`, `physical_io.rs`, `handle/tape_io/mod.rs`

- [ ] **Step 1: Add `DriveShared`, change the struct fields**

In `handle/mod.rs`, add (near the `DirtyState` definition):
```rust
use std::sync::{Arc, Mutex};

/// The mutable state a drive shares with its library: the audit hook and the
/// dirty-state machine. Held behind `Arc<Mutex<_>>` so a `DriveHandle` can own a
/// clone instead of `&mut`-borrowing the `LibraryHandle` (Phase 1 — lifts the
/// single-open-drive constraint). Locks are brief (per audit-fire / dirty-mark),
/// never held across a CDB.
#[derive(Default)]
pub(crate) struct DriveShared {
    pub(crate) audit_hook: Option<AuditHook>,
    pub(crate) dirty: DirtyState,
}
```
Change `LibraryHandle`'s `audit_hook` + `dirty` fields to:
```rust
    shared: Arc<Mutex<DriveShared>>,
```
and change `DriveHandle<'a>`'s `audit_hook: &'a mut …` + `dirty: &'a mut …` to:
```rust
    shared: Arc<Mutex<DriveShared>>,
```
Make `DriveHandle` non-generic: `pub struct DriveHandle {` (drop `<'a>`), and drop `<'a>` from its `impl` blocks (the Debug impl ≈ :1259, `impl DriveHandle` ≈ :1278) and from `impl super::DriveHandle` in `handle/tape_io/mod.rs:255`.

- [ ] **Step 2: Initialize `shared` where the library is built**

Find where `LibraryHandle { … }` is constructed (the `open_with`/`from_*` path that currently sets `audit_hook: None, dirty: DirtyState::default()`) and replace those two with:
```rust
    shared: Arc::new(Mutex::new(DriveShared::default())),
```

- [ ] **Step 3: `open_drive` clones the Arc**

In `open_drive` (≈ :756-763), replace the `audit_hook: &mut self.audit_hook,` and `dirty: &mut self.dirty,` fields of the returned `DriveHandle` with:
```rust
        shared: self.shared.clone(),
```
Change the return type `Result<DriveHandle<'_>, OpenError>` → `Result<DriveHandle, OpenError>`. (The `library_serial` clone and the rest are unchanged.)

- [ ] **Step 4: Lock every audit/dirty access (mechanical, uniform)**

Apply this transformation at each site listed in Context:
- `fire_audit(&mut self.audit_hook, &event)` → `fire_audit(&mut self.shared.lock().expect("drive shared lock").audit_hook, &event)` (the temporary `MutexGuard` drops at the end of the statement — lock held only for the call). Same for `fire_refused(&mut self.audit_hook, …)` and `fire_warnings(&mut self.audit_hook, …)`.
- `self.dirty.mark(cause)` → `self.shared.lock().expect("drive shared lock").dirty.mark(cause)`; `self.dirty.clear()` / `self.dirty.is_dirty()` / `self.dirty.cause()` likewise.
- `set_audit_hook` (:230): body becomes `self.shared.lock().expect("drive shared lock").audit_hook = Some(Box::new(hook));`.
- `LibraryHandle::is_dirty`/`cause` and `DriveHandle::is_dirty`/`dirty_cause`: `self.shared.lock().expect("…").dirty.is_dirty()` / `.cause()`.
- `DriveHandle::mark_dirty` (≈ :254 / its drive-side equivalent ≈ :1385) and `clear_dirty`: same `self.shared.lock()…dirty.…` form.

Representative example — `move_medium_as` Started fire + (Ok branch) dirty-mark + Finished fire become three independent locked statements with the `self.transport.execute_none(&cdb)` call between them holding **no** lock:
```rust
    fire_audit(&mut self.shared.lock().expect("drive shared lock").audit_hook,
               &AuditEvent::Started { /* … */ });
    self.transport.set_timeout_for(TimeoutClass::Move);
    let result = self.transport.execute_none(&cdb);          // <-- no lock held here
    // … on Ok:
    if touches_ie {
        self.shared.lock().expect("drive shared lock").dirty.mark(DirtyCause::VendorSemantics);
    }
    fire_audit(&mut self.shared.lock().expect("drive shared lock").audit_hook,
               &AuditEvent::Finished { /* … */ });
```
Use `.expect("drive shared lock")` consistently (a poisoned lock means a prior holder panicked — fail loudly; matches the single-threaded reality of Phase 1).

- [ ] **Step 5: Wrapper lifetime arity (same crate)**

`block_io.rs`: `pub struct DriveHandleSink<'a, 'b>(pub &'a mut DriveHandle<'b>);` → `pub struct DriveHandleSink<'a>(pub &'a mut DriveHandle);` and its `impl<'a, 'b> … for DriveHandleSink<'a, 'b>` → `impl<'a> … for DriveHandleSink<'a>`. Same for `DriveHandleSource` (:118). `physical_io.rs`: `DriveHandlePhysicalSource<'a, 'b>` → `<'a>` (struct + impls).

- [ ] **Step 6: Add the coexistence test (the proof)**

In `handle/mod.rs` `#[cfg(test)] mod tests` (which already builds a `LibraryHandle` from fixture transports — mirror the existing `open_drive` test setup), add:
```rust
    #[test]
    fn two_drives_open_simultaneously() {
        let mut library = /* build a LibraryHandle with >=2 drive bays via the
                             existing fixture-transport helper used by the other
                             open_drive tests */;
        let bay_a = /* first drive bay element address from the fixture */;
        let bay_b = /* second drive bay element address from the fixture */;
        let policy = StaticAllowlist::new(["<fixture library serial>"]);

        let drive_a = library.open_drive(bay_a, &policy).expect("open drive A");
        let drive_b = library.open_drive(bay_b, &policy).expect("open drive B");

        // Both live at once — this does NOT compile under the old `<'a>` borrow.
        assert_eq!(drive_a.bay_address(), bay_a);
        assert_eq!(drive_b.bay_address(), bay_b);
        // Library still usable while both drives are open.
        let _ = library.is_dirty();
    }
```
(Use the same fixture-transport factory the existing `open_drive` / `load`/`unload` tests use; pick two distinct drive-bay addresses present in that fixture. If `bay_address()` isn't public, assert via another observable accessor — or add `pub fn bay_address(&self) -> u16`.)

- [ ] **Step 7: Build + test the library**

Run: `cargo test -p remanence-library`
Expected: PASS — all existing handle/block_io/physical_io tests (behavior-neutral) **plus** `two_drives_open_simultaneously`.

- [ ] **Step 8: Commit**

```bash
git add crates/remanence-library/src/
git commit -m "Layer2 P1: share audit_hook+dirty via Arc<Mutex>; own the DriveHandle"
```

---

## Task 2: Lifetime ripple in `remanence-parity`

**Files:** `crates/remanence-parity/src/raw.rs`

- [ ] **Step 1: Drop the inner `DriveHandle` lifetime from the raw wrappers**

`DriveHandleRawSource<'a, 'b>` holding `drive: &'a mut DriveHandle<'b>` → `DriveHandleRawSource<'a>` with `drive: &'a mut DriveHandle`; update `pub fn new(drive: &'a mut DriveHandle) -> Self` and the `impl<'a, 'b>` → `impl<'a>`. Same for `DriveHandleRawSink<'a, 'b>` (:304-310).

- [ ] **Step 2: Build + test**

Run: `cargo test -p remanence-parity`
Expected: PASS (the raw source/sink + scan tests still compile and pass).

- [ ] **Step 3: Commit**

```bash
git add crates/remanence-parity/src/raw.rs
git commit -m "Layer2 P1: drop inner DriveHandle lifetime from parity raw wrappers"
```

---

## Task 3: Signature ripple in `remanence-api` + `remanence-cli`

**Files:** `crates/remanence-api/src/{mount.rs, write_owner.rs}`, `crates/remanence-cli/src/lib.rs`

- [ ] **Step 1: `load_tape_by_uuid` returns an owned `DriveHandle`**

In `mount.rs`, change `pub fn load_tape_by_uuid<'a>(index: …, library: &'a mut LibraryHandle, policy: …, tape_uuid: …) -> Result<DriveHandle<'a>, LoadByUuidError>` to drop the lifetime: `…(library: &mut LibraryHandle, …) -> Result<DriveHandle, LoadByUuidError>`. The body is unchanged (it still `library.load(...)` + `library.open_drive(...)`; the returned handle simply no longer borrows `library`).

- [ ] **Step 2: Fix the remaining `DriveHandle<'_>` references**

`write_owner.rs:1281,1363`: `drive: &mut DriveHandle<'_>` → `drive: &mut DriveHandle`. `remanence-cli/src/lib.rs:3771,4137`: `drive: &mut remanence_library::DriveHandle<'_>` → `&mut remanence_library::DriveHandle`. No other logic changes — the call sites (`DriveHandleSink(&mut drive)`, `load_tape_by_uuid(index, &mut library, …)`) keep working; `drive` is now owned rather than borrowing `library`, which only relaxes lifetimes.

- [ ] **Step 3: Build the workspace**

Run: `cargo build --workspace`
Expected: clean (this is the first point the whole workspace compiles again after the lifetime removal).

- [ ] **Step 4: Commit**

```bash
git add crates/remanence-api/src/ crates/remanence-cli/src/lib.rs
git commit -m "Layer2 P1: ripple owned-DriveHandle signatures through api + cli"
```

---

## Task 4: Workspace gates

- [ ] **Step 1: Format** — `cargo fmt --all`; review `git diff --stat`.
- [ ] **Step 2: Clippy** — `cargo clippy --workspace --all-targets -- -D warnings` (expect clean).
- [ ] **Step 3: Test** — `cargo test --workspace` (expect PASS: the full S1–S2 + Layer 1–5 regression, behavior-neutral, plus `two_drives_open_simultaneously`).
- [ ] **Step 4: Commit** — `git add -A && git commit -m "Layer2 P1: fmt" || echo "nothing to format"`.

---

## Self-Review (completed during planning)

**Spec coverage:** `DriveShared` + `Arc<Mutex>` on `LibraryHandle` (Task 1.1-2) ✓; owned `DriveHandle` + `open_drive` clone (Task 1.1, 1.3) ✓; brief-lock every audit/dirty access, never across a CDB (Task 1.4 + the safety note) ✓; lifetime ripple across library/parity/api/cli (Tasks 1.5, 2, 3) ✓; coexistence proof test (Task 1.6) ✓; behavior-neutral regression (Task 4.3) ✓. OUT items (using two drives, ChangerHandle, Send) are untouched.

**Placeholder scan:** the only non-literal spots are the fixture-transport setup + bay addresses in the Task 1.6 test (which must match the existing `open_drive` test fixtures in that module — pointed at, not invented) and the enumerated mechanical sweep in Task 1.4 (a single uniform transformation with a worked example + an exhaustive site list). Both are precise instructions, not hand-waving.

**Type consistency:** `shared: Arc<Mutex<DriveShared>>` is identical on both handles; `DriveShared { audit_hook: Option<AuditHook>, dirty: DirtyState }` reuses the existing types unchanged; `open_drive` returns `DriveHandle` (no lifetime), matched by `load_tape_by_uuid`'s new return and the `&mut DriveHandle` references; wrapper types keep their borrow lifetime (`<'a>`) and lose only the inner one (`<'b>`).
