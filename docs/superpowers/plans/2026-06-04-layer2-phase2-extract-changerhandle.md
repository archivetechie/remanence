# Layer 2 Phase 2 — extract `ChangerHandle` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Factor a pure-changer `ChangerHandle { library, transport, shared }` out of `LibraryHandle`, leaving `LibraryHandle { changer, transport_factory }` as a thin facade. Behavior-neutral; `remanence-library`-internal (consumers unchanged via delegation).

**Architecture:** Move the changer-only methods onto `ChangerHandle`; keep `open_drive` + composed `load`/`unload` on the `LibraryHandle` facade, routing the changer parts through `self.changer`. `Library::open_with` builds the changer then wraps it.

**Tech Stack:** Rust, `remanence-library` Layer 2 (`std::sync::{Arc, Mutex}`).

---

## Context the implementer needs

- **Design doc:** `docs/layer2-phase2-extract-changerhandle-design-v0.1.md`; umbrella: `docs/multidrive-concurrency-architecture-v0.1.md`. Builds on Phase 1 (owned `DriveHandle`).
- **Current `LibraryHandle`** (`crates/remanence-library/src/handle/mod.rs`): struct at :84 = `{ library, transport, transport_factory, shared: Arc<Mutex<DriveShared>> }`. `impl LibraryHandle` at :218. Methods: `library()`:220, `set_audit_hook`:227, `is_dirty`:240, `dirty_cause`:253, `mark_dirty`:259, `clear_dirty`:270, `refresh`:300, `rescan`:370, `move_medium`:523 (+ `move_medium_as`), `open_drive`:695, `load`:827, `unload`:891, `export`:976, `import`:1017, `lock_removal`:1064, `allow_removal`:1075, `issue_prevent_allow`:1082. `Debug` impl :199. Construction: `Library::open`:1537 / `open_with`:1561 (builds `LibraryHandle { … }` at :1628). There is a `Deref`/`DerefMut` to `LibraryHandle` at :1194/:1205 (the `RemovalLockGuard` or similar) — keep its target type `LibraryHandle`.
- **The split** (from the verified skeleton):
  - **`ChangerHandle`** owns `{ library, transport, shared }` and gets: `library`, `set_audit_hook`, `is_dirty`, `dirty_cause`, `mark_dirty`, `clear_dirty`, `refresh`, `rescan`, `move_medium` (+ `move_medium_as`), `export`, `import`, `lock_removal`, `allow_removal`, `issue_prevent_allow`, and the changer-side helpers they call (RES/INIT issue helpers, `fire_audit`/`fire_refused`/`fire_warnings` stay free fns). Their bodies are **unchanged** — `self.transport`/`self.library`/`self.shared` now refer to `ChangerHandle`'s own fields.
  - **`LibraryHandle`** becomes `{ changer: ChangerHandle, transport_factory }` and keeps `open_drive`, `load`, `unload`, plus new `changer(&self) -> &ChangerHandle`, `changer_mut(&mut self) -> &mut ChangerHandle`, `into_changer(self) -> ChangerHandle`, and one-line **delegating** methods for every changer op a consumer or test calls on a `LibraryHandle` (so call sites are unchanged).
- **Consumers** call `LibraryHandle` methods only (`mount.rs`, `cli`, tests) — preserved by delegation, so **no changes outside `remanence-library`** are expected. (If a delegating method is missing for something a consumer calls, add it.)

## File Structure

- **Modify** `crates/remanence-library/src/handle/mod.rs` — define `ChangerHandle`; move changer ops; reshape `LibraryHandle`; `open_with` construction; `Debug` impls; delegation + accessors.
- **Modify** `crates/remanence-library/src/lib.rs` — re-export `ChangerHandle` (`pub use handle::{…, ChangerHandle}`).
- **Possibly** `crates/remanence-library/src/handle/tape_io/mod.rs` / `block_io.rs` / `physical_io.rs` — only if they reference the moved `LibraryHandle` fields directly (they operate on `DriveHandle`, so likely untouched).

---

## Task 1: Extract `ChangerHandle`, reshape `LibraryHandle` as a facade

**Files:** `crates/remanence-library/src/handle/mod.rs`, `crates/remanence-library/src/lib.rs`

- [ ] **Step 1: Define `ChangerHandle` and move the changer ops**

Add:
```rust
/// The library's medium changer + inventory: the robot, the slot/drive-bay
/// snapshot, and the shared audit/dirty cell. Issues all changer CDBs (MOVE
/// MEDIUM, READ/INIT ELEMENT STATUS, PREVENT/ALLOW). This is the unit the
/// Layer-5 changer actor owns; it is `Send`.
pub struct ChangerHandle {
    library: Library,
    transport: Box<dyn SgTransport>,
    shared: Arc<Mutex<DriveShared>>,
}
```
Move these from `impl LibraryHandle` into `impl ChangerHandle`, bodies **verbatim**: `library`, `set_audit_hook`, `is_dirty`, `dirty_cause`, `mark_dirty`, `clear_dirty`, `refresh`, `rescan`, `move_medium`, `move_medium_as`, `export`, `import`, `lock_removal`, `allow_removal`, `issue_prevent_allow`. (Their `self.transport` / `self.library` / `self.shared` now bind to `ChangerHandle`'s fields — no edits to the bodies.) Note: `lock_removal` returns `RemovalLockGuard<'_>` — its `Deref` target stays whatever it is today; if that guard derefs to `LibraryHandle`, retarget it to `ChangerHandle` (it only needs the changer). Move the changer-side `Debug` fields into a `ChangerHandle` `Debug` impl.

- [ ] **Step 2: Reshape `LibraryHandle` as a facade**

```rust
pub struct LibraryHandle {
    changer: ChangerHandle,
    transport_factory: TransportFactory,
}

impl LibraryHandle {
    pub fn changer(&self) -> &ChangerHandle { &self.changer }
    pub fn changer_mut(&mut self) -> &mut ChangerHandle { &mut self.changer }
    pub fn into_changer(self) -> ChangerHandle { self.changer }

    // Delegating methods — keep the existing public surface for consumers/tests.
    pub fn library(&self) -> &Library { self.changer.library() }
    pub fn set_audit_hook<F>(&mut self, hook: F) where F: FnMut(&AuditEvent<'_>) + Send + 'static { self.changer.set_audit_hook(hook) }
    pub fn is_dirty(&self) -> bool { self.changer.is_dirty() }
    pub fn dirty_cause(&self) -> Option<DirtyCause> { self.changer.dirty_cause() }
    pub fn refresh(&mut self) -> Result<(), ScsiError> { self.changer.refresh() }
    pub fn rescan(&mut self) -> Result<(), RescanError> { self.changer.rescan() }
    pub fn move_medium(&mut self, src: u16, dst: u16, policy: &dyn AccessPolicy) -> Result<(), MoveError> { self.changer.move_medium(src, dst, policy) }
    pub fn export(&mut self, slot: u16, policy: &dyn AccessPolicy) -> Result<(), MoveError> { self.changer.export(slot, policy) }
    pub fn import(&mut self, slot: u16, policy: &dyn AccessPolicy) -> Result<(), MoveError> { self.changer.import(slot, policy) }
    pub fn lock_removal(&mut self) -> Result<RemovalLockGuard<'_>, ScsiError> { self.changer.lock_removal() }
    pub fn allow_removal(&mut self) -> Result<(), ScsiError> { self.changer.allow_removal() }
    // open_drive / load / unload kept below (Step 3).
}
```
(Match the real method signatures exactly — copy them from the current `impl LibraryHandle`. Add a delegating method for any other public changer op a consumer/test calls; omit ones nobody calls externally — `changer_mut()` covers internal needs.)

- [ ] **Step 3: Keep `open_drive` / `load` / `unload` on the facade, routed through `self.changer`**

`open_drive`: unchanged except the returned `DriveHandle`'s `shared` comes from `self.changer.shared.clone()` and the drive transport still from `(self.transport_factory)(…)`. `load`/`unload`: replace internal `self.move_medium_as(…)` calls with `self.changer.move_medium_as(…)`, and any `self.shared`/audit/dirty access with `self.changer.<…>` (or `self.changer.mark_dirty(…)`); the `self.open_drive(bay, policy)?` call and the `drive.load_as(…)`/`drive.unload_as(…)` steps are unchanged. The sequential `&mut self` borrows (changer MOVE, then `open_drive`) are released in turn — verified.

- [ ] **Step 4: Restructure construction in `open_with`**

At `:1628`, replace `Ok(LibraryHandle { library, transport, transport_factory, shared })` with:
```rust
        Ok(LibraryHandle {
            changer: ChangerHandle { library, transport, shared },
            transport_factory,
        })
```
(`shared` is the `Arc::new(Mutex::new(DriveShared::default()))` already built in Phase 1.)

- [ ] **Step 5: Re-export `ChangerHandle`**

In `crates/remanence-library/src/lib.rs`, add `ChangerHandle` to the `pub use handle::{…}` list.

- [ ] **Step 6: Build the library**

Run: `cargo check -p remanence-library`
Expected: clean. Resolve any "no method on `LibraryHandle`" by adding the missing delegating method (Step 2) or routing an internal caller through `self.changer`.

- [ ] **Step 7: Add the Phase-2 test**

In `handle/mod.rs` `mod tests` (reuse the existing fixture-transport `LibraryHandle` setup), add:
```rust
    #[test]
    fn changer_handle_is_usable_standalone() {
        let mut library = /* existing fixture LibraryHandle with a slot + free bay */;
        let policy = StaticAllowlist::new(["<fixture serial>"]);
        // Drive the changer surface through the extracted handle.
        let changer = library.changer_mut();
        changer.move_medium(/* src slot */, /* dst */, &policy).expect("changer move");
        assert!(/* observable post-move state via changer.library() */);
    }

    fn _changer_is_send() { fn ss<T: Send>() {} ss::<ChangerHandle>(); }
```
(Mirror the element addresses from the existing `move_medium`/`load` tests in this module.)

- [ ] **Step 8: Test + commit**

Run: `cargo test -p remanence-library` (expect PASS — all existing handle/block_io/physical_io tests behavior-neutral, the Phase-1 `two_drives_open_simultaneously`, and the new `changer_handle_is_usable_standalone`).
```bash
git add crates/remanence-library/src/handle/mod.rs crates/remanence-library/src/lib.rs
git commit -m "Layer2 P2: extract ChangerHandle; LibraryHandle becomes a facade"
```

---

## Task 2: Workspace verification

- [ ] **Step 1: Build the workspace** — `cargo build --workspace` (expect clean — consumers unchanged thanks to delegation; if a consumer or a sibling module referenced a moved field, route it through `library.changer()` / `changer_mut()`).
- [ ] **Step 2: Format** — `cargo fmt --all`; review `git diff --stat`.
- [ ] **Step 3: Clippy** — `cargo clippy --workspace --all-targets -- -D warnings` (expect clean).
- [ ] **Step 4: Test** — `cargo test --workspace` (expect PASS: full Layer 1-5 + S1-S2 regression + the new test).
- [ ] **Step 5: Commit** — `git add -A && git commit -m "Layer2 P2: fmt + downstream fixups" || echo "nothing to commit"`.

---

## Self-Review (completed during planning)

**Spec coverage:** `ChangerHandle` struct + moved changer ops (Task 1.1) ✓; `LibraryHandle` facade + delegation + accessors + `into_changer` (Task 1.2) ✓; `open_drive`/`load`/`unload` routed through `self.changer` (Task 1.3) ✓; `open_with` construction (Task 1.4) ✓; re-export (Task 1.5) ✓; standalone-changer test + Send assert (Task 1.7) ✓; behavior-neutral regression (Task 2.4) ✓. OUT items (open_changer, independent DriveHandle ctor, the mount module/actors) untouched.

**Placeholder scan:** the test's fixture setup + element addresses (Task 1.7) are pointed at the existing tests in the module, not invented. The delegating-method list (Task 1.2) is "every changer op a consumer/test calls" with the signatures copied from the current `impl LibraryHandle` — explicit, with a catch-all instruction for any missed one.

**Type consistency:** `ChangerHandle { library, transport, shared }` holds exactly the three fields the changer ops use; `LibraryHandle { changer, transport_factory }`; delegating method signatures are identical to today's (copied), so consumer call sites bind unchanged; `into_changer(self) -> ChangerHandle` gives Phase 3 an owned changer.
