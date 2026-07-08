# Layer 2 — Phase 2: extract `ChangerHandle` from `LibraryHandle` Design v0.1

Status: design decision. Second phase of the multi-drive concurrency architecture
(`docs/multidrive-concurrency-architecture-v0.1.md`); builds on Phase 1 (owned
`DriveHandle`, ✅ done). Goal: factor out a **pure-changer `ChangerHandle`** — the
type the Phase-3 ChangerActor will own — leaving `LibraryHandle` as a thin facade.
Behavior-neutral; no concurrency turned on.

## Background

Post-Phase-1, `LibraryHandle` holds `{ library, transport (changer /dev/sg),
transport_factory, shared: Arc<Mutex<DriveShared>> }` and a surface that is
**mostly changer ops** — `move_medium`, `export`, `import` (all changer-only
MOVEs), `refresh`, `rescan`, `lock_removal`/`allow_removal`, the inventory
accessor `library()`, and the dirty/audit accessors — plus three drive-spanning
methods: `open_drive` (opens a `DriveHandle` via the factory) and the composed
`load`/`unload` (changer MOVE + drive SSC). For Phase 3, the ChangerActor needs a
cohesive type that does *only* the changer ops, decoupled from drive-opening.

## Design — `ChangerHandle` core + `LibraryHandle` facade

```rust
/// Pure changer: the robot + the library inventory. What the ChangerActor owns.
pub struct ChangerHandle {
    library: Library,
    transport: Box<dyn SgTransport>,          // changer /dev/sg
    shared: Arc<Mutex<DriveShared>>,
}
// moves here: library(), set_audit_hook, is_dirty/dirty_cause/mark_dirty/clear_dirty,
// refresh, rescan, move_medium (+ move_medium_as), export, import,
// lock_removal/allow_removal/issue_prevent_allow, and the RES/INIT helpers.

/// Facade: the changer + drive-opening + the composed drive-spanning ops.
pub struct LibraryHandle {
    changer: ChangerHandle,
    transport_factory: TransportFactory,
}
// keeps: open_drive (factory + self.changer.shared.clone()); the composed
// load/unload (self.changer.move_medium_as + open_drive + drive SSC); plus
// `changer(&self) -> &ChangerHandle`, `changer_mut(&mut self) -> &mut ChangerHandle`,
// `into_changer(self) -> ChangerHandle`, and thin delegating methods for the
// changer ops external consumers call (so their call sites are unchanged).
```

- **Construction:** `Library::open_with` builds the `ChangerHandle { library,
  transport, shared }` then wraps it: `LibraryHandle { changer, transport_factory }`.
  `Library::open` / `open_with` keep their signatures (return `LibraryHandle`).
- **Composed `load`/`unload` (unchanged behavior):** `load(slot, bay, policy)` =
  `self.changer.move_medium_as(slot, bay, policy, AuditOp::Load{..})` → `let drive
  = self.open_drive(bay, policy)?` → `drive.load_as(..)`. Sequential `&mut self`
  borrows (the changer MOVE, then the open) — each released before the next
  (verified). The drive's SSC fires into `self.changer.shared` (its Arc clone), so
  the unified audit/dirty stream is preserved exactly.
- **Delegation:** any changer op a consumer currently calls on `LibraryHandle`
  (e.g. `move_medium`, `refresh`, `export`, `import`, `is_dirty`, `set_audit_hook`)
  gets a one-line forwarding method on the facade, so `remanence-api`/`-cli`/tests
  are unchanged. (Internally those just call `self.changer.<op>`.)
- **`ChangerHandle` is `pub`** (re-exported from `remanence-library`) so the
  Phase-3 ChangerActor in `remanence-api` can own one (via `LibraryHandle::into_changer`
  or, in Phase 3, a dedicated `Library::open_changer`). It is `Send` (verified) —
  a bonus for the actor phase, not required here.

## Why behavior-neutral

The split is a pure code move: the same `library`/`transport`/`shared` and the
same method bodies, now homed on `ChangerHandle`, with `LibraryHandle` forwarding.
No method changes what CDB it issues, what it audits, or what it marks dirty. The
public `LibraryHandle` API is preserved by the delegating methods + accessors.

## Scope

**IN:** the `ChangerHandle` struct + moving the changer ops onto it; the
`LibraryHandle` facade (`{ changer, transport_factory }`) keeping `open_drive` +
composed `load`/`unload` + `changer()`/`changer_mut()`/`into_changer()` +
delegating methods; `open_with` construction restructure; internal field-access
ripple (`self.transport`→`self.changer.transport`, `self.shared`→`self.changer.shared`,
`self.library`→`self.changer.library` inside moved/composed methods).

**OUT:** a standalone `Library::open_changer()` that opens the changer *without*
the drive factory (Phase 3, when the ChangerActor wants one directly); an
independent `DriveHandle` constructor that needs no `LibraryHandle` (Phase 3); the
per-drive reservation / actor threads / `mount` module (Phase 3); moving
`load`/`unload` composition out of `LibraryHandle` into the `mount` module
(Phase 3); any behavior change.

## Acceptance criteria

1. **Regression (behavior-neutral):** the full `remanence-library`/`-parity`/
   `-api`/`-cli` suites stay green — including the Layer-2b/3a handle tests
   (move/load/unload/export/import, refresh/rescan, audit-hook + dirty) and the
   Phase-1 `two_drives_open_simultaneously` test.
2. **New:** a `handle` unit test that obtains a `ChangerHandle` (via
   `library.changer_mut()` and/or `into_changer()`), issues a changer op
   (e.g. `move_medium` over the fixture transport) through it, and asserts the
   result — proving the changer surface is usable as a standalone type. Plus a
   `fn _assert(){ fn ss<T:Send>(){} ss::<ChangerHandle>(); }` style Send assert
   (compile-time).
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-library` + `cargo clippy -p
remanence-library --all-targets -- -D warnings` (both clean) on 2026-06-04 with a
standalone skeleton (`crates/remanence-library/src/p2_skeleton.rs`, since removed
— design-only): a pure-changer `ChangerHandle` (transport + inventory + shared +
`move_medium`/`refresh`/`is_dirty`), a `LibraryHandle { changer, transport_factory }`
facade with delegating `move_medium`, `open_drive` (cloning `self.changer.shared`),
and a composed `load` (changer MOVE → `open_drive` → drive SSC) whose sequential
`&mut self` borrows compile; `ChangerHandle`/`DriveHandle` both `Send`.

Five-category result:
1. **Module privacy** — `ChangerHandle` is `pub` (re-exported); the moved methods
   keep their visibility; `DriveShared` stays `pub(crate)`. Pass.
2. **`!Send` in threading** — `ChangerHandle` is `Send` (confirmed) — owns the same
   Send-able fields; not required in Phase 2 but sets up Phase 3. Pass.
3. **Reactor timing** — none (no async touched). Pass.
4. **Borrowed-handle plumbing** — the crux: `load`/`unload` do two *sequential*
   `&mut self` borrows (`self.changer.move_medium_as`, then `self.open_drive`),
   each released before the next; no simultaneous double-borrow. Verified. Pass.
5. **Trait/method visibility** — `Library::open_with` builds `ChangerHandle` then
   `LibraryHandle`; `std::sync::{Arc,Mutex}`, `TransportFactory`, `DriveShared`,
   `SgTransport` in scope. Pass.

No new dependencies.
