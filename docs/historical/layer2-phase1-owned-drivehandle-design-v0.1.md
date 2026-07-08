# Layer 2 — Phase 1: own the `DriveHandle` (lift the single-drive lifetime constraint) Design v0.1

Status: design decision. First phase of the multi-drive concurrency architecture
(`docs/multidrive-concurrency-architecture-v0.1.md`). Goal: **remove the
lifetime coupling that forbids two open drives per library**, behavior-neutrally,
so the later supervisor-tree phases can hold N `DriveHandle`s. No concurrency is
turned on here — this is the enabling Layer 2 refactor.

## Background — the one coupling, and why lifting it is safe

`DriveHandle<'a>` borrows exactly two things from `LibraryHandle`
(`handle/mod.rs:1236`): `audit_hook: &'a mut Option<AuditHook>` and
`dirty: &'a mut DirtyState`. That `&'a mut` is the *only* reason two drives can't
be open at once (it does **not** borrow the changer transport — each drive owns
its own — nor the inventory snapshot — it carries a clone). The borrow exists so
drive-side ops (SSC LOAD/UNLOAD, Layer 3a I/O completion-unknown) fire audit into
the same hook and mark the same dirty state as changer ops — *unification by
shared borrow*.

Two facts make lifting it safe:
- **The daemon never uses either.** `write_owner` opens the library with no audit
  hook installed (the Layer-2 `set_audit_hook` is called only in tests/CLI), and
  never reads `is_dirty()` (audit + op-state go through the S3a operations path).
  So for the daemon, audit_hook is `None` and dirty is write-only — sharing vs.
  borrowing is invisible.
- **The CLI/tests do use them**, with one library + one drive at a time, and rely
  on drive ops reaching the library's hook + dirty. Preserving *that* (not the
  borrow) is the only behavior requirement.

## Design — share the cell, drop the lifetime

Replace the `&'a mut` borrow with a **shared owned cell**:

```rust
// handle/mod.rs
pub(crate) struct DriveShared {
    audit_hook: Option<AuditHook>,
    dirty: DirtyState,
}

pub struct LibraryHandle {
    library: Library,
    transport: Box<dyn SgTransport>,
    transport_factory: TransportFactory,
    shared: Arc<Mutex<DriveShared>>,   // was: audit_hook + dirty as direct fields
}

pub struct DriveHandle {               // was: DriveHandle<'a>
    bay_address: u16,
    drive: InstalledDrive,
    library_serial: String,
    transport: Box<dyn SgTransport>,
    shared: Arc<Mutex<DriveShared>>,   // was: &'a mut audit_hook + &'a mut dirty
}
```

- `open_drive(&mut self, …) -> Result<DriveHandle, OpenError>` clones the Arc
  (`shared: self.shared.clone()`) instead of borrowing — the returned handle is
  **owned**, tied to `self` only for the duration of the call, so the library
  stays usable and **two `DriveHandle`s coexist** (verified — §Verification).
- Every `self.audit_hook` / `self.dirty` access (in `move_medium_as`, `load`,
  `unload`, `export`, `import`, `refresh`, `rescan`, `set_audit_hook`,
  `is_dirty`, `cause`, and the `DriveHandle` SSC/Layer-3a paths) becomes a
  **brief lock** on `self.shared`: `let mut g = self.shared.lock()…; …g.audit_hook
  / g.dirty…`. The lock is taken per audit-fire / per dirty-mark and released
  immediately — **never held across a CDB execution** (a MOVE is 8–20 s). The
  `fire_audit`/`fire_refused`/`fire_warnings` helpers keep their `&mut
  Option<AuditHook>` signatures; call sites pass `&mut self.shared.lock()…
  .audit_hook` (the temporary guard drops at the end of the statement).
- `DirtyState` is unchanged; it just lives inside `DriveShared`.

Sharing via `Arc<Mutex<DriveShared>>` **preserves today's semantics exactly**:
the library and its drive(s) see one audit hook + one dirty state, so unified
audit and dirty-propagation are intact for the CLI/tests. The only observable
change is a *loosening*: `set_audit_hook` may now be called while a drive is open
(previously a borrow conflict), and N drives may be opened — neither breaks
existing callers.

### Lifetime ripple (mechanical)

Dropping `DriveHandle`'s `'a` ripples through every `DriveHandle<'…>` site; the
*wrapper* types keep their own borrow lifetime but lose the inner one:
- `handle/mod.rs`: `struct DriveHandle<'a>` → `DriveHandle`; `impl<'a> …
  DriveHandle<'a>` (incl. Debug, the Layer-3a `impl` in `handle/tape_io/mod.rs`)
  → non-generic; `open_drive` return `DriveHandle<'_>` → `DriveHandle`.
- `block_io.rs`: `DriveHandleSink<'a, 'b>(&'a mut DriveHandle<'b>)` →
  `DriveHandleSink<'a>(&'a mut DriveHandle)`; same for `DriveHandleSource`.
- `physical_io.rs`: `DriveHandlePhysicalSource<'a, 'b>` → `<'a>`.
- `remanence-parity/src/raw.rs`: `DriveHandleRawSource<'a, 'b>` /
  `DriveHandleRawSink<'a, 'b>` (hold `&'a mut DriveHandle<'b>`) → `<'a>`.
- `remanence-api` (`mount.rs`): `load_tape_by_uuid<'a>(…) -> DriveHandle<'a>` →
  `load_tape_by_uuid(…) -> DriveHandle` (still takes `&mut LibraryHandle` to open,
  but the return no longer borrows it); `write_owner.rs`: `&mut DriveHandle<'_>`
  → `&mut DriveHandle`.
- `remanence-cli`: `&mut remanence_library::DriveHandle<'_>` → `&mut …DriveHandle`.

## Concurrency note (forward-looking)

The `Mutex<DriveShared>` is uncontended today (single owner) and, in the Phase-3
actor model, is taken only briefly per audit-fire / dirty-mark — never during
I/O — so it is not a throughput path. (When `ChangerHandle` is extracted in
Phase 2, the changer and per-drive dirty/audit may split into separate cells;
Phase 1 keeps one shared cell to stay behavior-neutral.)

## Scope

**IN:** the `DriveShared` cell + `Arc<Mutex>` on `LibraryHandle`; `DriveHandle`
loses its lifetime and owns an Arc clone; `open_drive` clones; all
`audit_hook`/`dirty` accesses become brief locks; the lifetime ripple across
`block_io`/`physical_io`/`parity::raw`/`mount`/`write_owner`/`cli`; a test
proving two `DriveHandle`s coexist.

**OUT:** actually *using* two drives concurrently (the daemon still opens one at a
time — that's Phase 3); extracting `ChangerHandle` (Phase 2); per-drive vs.
changer dirty/audit split (Phase 2); making `DriveHandle`/`LibraryHandle` `Send`
for cross-thread moves (not needed — actors build in-thread; Phase 3); any
behavior change to audit or dirty semantics.

## Acceptance criteria

1. **Regression (behavior-neutral):** the full existing `remanence-library`,
   `-parity`, `-api`, `-cli` suites stay green — proving the Arc<Mutex> share +
   lifetime removal changed no behavior. In particular the Layer-2b/3a handle
   tests that install an audit hook on the library and assert drive-side ops
   appear in it (and that drive ops mark `library.is_dirty()`) still pass.
2. **New (the proof):** a `handle` unit test opens two `DriveHandle`s from one
   `LibraryHandle` (via a fixture transport) and holds both live simultaneously
   — which does not compile today. Asserts both are usable (e.g., each reports
   its `bay_address`) and the library is still usable while both are open.
- Gates: `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test`.

## §verification — Rust design verification

Verified against `cargo check -p remanence-library` + `cargo clippy -p
remanence-library --all-targets -- -D warnings` (both clean) on 2026-06-04 with a
standalone skeleton (`crates/remanence-library/src/p1_skeleton.rs`, since removed
— design-only): a lifetime-free `DriveHandle` owning `Arc<Mutex<DriveShared>>`
(stand-ins mirroring `Box<dyn FnMut + Send>` + a plain dirty struct), an
`open_drive(&mut self) -> DriveHandle` that clones the Arc, **two coexisting
`DriveHandle`s with the changer still usable**, and `assert_send::<DriveHandle>()`.

Five-category result:
1. **Module privacy** — `DriveShared` is `pub(crate)` in `handle`; `DirtyState`
   stays where it is. Pass.
2. **`!Send` in threading** — `Arc<Mutex<DriveShared>>` is Send+Sync
   (`Mutex<Box<dyn FnMut + Send>>: Sync`); the skeleton confirms `DriveHandle:
   Send` (a free bonus for Phase 3 — not required here). Pass.
3. **Reactor timing** — none (no async/tokio touched in Phase 1). Pass.
4. **Borrowed-handle plumbing** — the crux: the `&'a mut`→`Arc<Mutex>` clone
   removes the exclusive borrow so `open_drive` no longer ties the return to
   `self`; two handles coexist. Verified. Pass.
5. **Trait/method visibility** — `std::sync::{Arc, Mutex}`, `AuditHook`,
   `DirtyState` all in scope; wrapper types’ lifetime arity changes are local
   edits. Pass.

No new dependencies (`std::sync`).
