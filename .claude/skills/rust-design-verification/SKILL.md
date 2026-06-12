---
name: rust-design-verification
description: Use whenever authoring or substantively revising a Rust design document (anything that proposes new structs/traits/lifetimes/async patterns/module trees) for this project. Runs the five Rust-specific design pitfalls this codebase has hit, and requires a cargo-check-able skeleton before locking the design into a markdown doc.
---

# Rust Design Verification

This skill exists because, between 2026-05-17 and 2026-05-18, the Layer
2c and Layer 3a design docs went through **two full review cycles each**
to fix Rust-specific design errors that the markdown-only review process
couldn't catch. Codex eventually caught them — but only after the
design had been "approved" and (in Layer 2c's case) shipped as code.
The compiler would have caught every one of them in seconds.

The skill enforces one discipline (**write a compiling skeleton before
the markdown**) and a five-point checklist of the recurring categories.

## When to use this skill

**Auto-trigger** on tasks where you are:

- Writing or revising any `docs/layer*-design.md` doc.
- Adding new traits, structs, or impl blocks to a `*-design.md` doc.
- Proposing module-tree changes (`src/foo.rs` → `src/foo/`, child vs
  sibling module placement).
- Designing an API that crosses crate boundaries, holds borrowed
  references with lifetimes, or uses tokio/async patterns.
- Choosing between Cargo crates as the unit of layering.

**Skip** if:

- The doc is operator/user-facing prose only (e.g. `INSTALL.md`,
  `JOURNAL.md`, `why-remanence.md`'s comparison sections) with no
  proposed Rust types in it.
- The change is removing or renaming types only — the compiler
  catches those during the implementation pass anyway.

When in doubt, run the skill. The cost is 15–30 minutes; the cost of
skipping when it would have helped is a full review cycle (see history
below).

## The fundamental discipline

**Before locking any Rust types into the design doc, write a minimal
compiling skeleton and run `cargo check`.** Just signatures, no
bodies. The compiler is the verification tool; the design doc is the
record of a verified decision.

The mechanical protocol:

1. **Identify** every new type, trait, lifetime, and method the design
   proposes.
2. **Stub them** in the actual target crate (`crates/remanence-library/src/...`
   for Layer 2/3a, etc.). The skeleton must **be compiled** for the
   verification to mean anything — otherwise the borrow checker, the
   `Send`/`Sync` checker, and the privacy checker never run on it.
   Acceptable annotations on stubs:
   - `#[allow(dead_code)]` — suppresses the unused-code warning while
     keeping the type/method in the compiled crate. **Default choice.**
   - `#[allow(unused_variables)]` / `#[allow(unused_imports)]` —
     targeted noise suppression for unused arguments and use
     statements.
   - Method bodies as `unimplemented!()` or `todo!()` — these compile
     (they return `!`), letting signatures and trait bounds be checked
     without supplying real logic.
   - **Do NOT** gate skeletons behind `#[cfg(any())]`, `#[cfg(false)]`,
     or any other always-false cfg. Such code is skipped by the
     compiler entirely — `cargo check` won't verify struct layouts,
     lifetimes, `Send`/`Sync` bounds, module privacy, or constructor
     plumbing. That produces a *false pass* on exactly the bugs this
     skill is meant to catch (codex review dd58abe9 surfaced this on
     the skill's first draft).
   - A real feature flag is fine if the skeleton needs to be hidden
     from a default build (`#[cfg(feature = "verify-skeleton")]`),
     but the verification cargo-check must include `--features
     verify-skeleton` so the code actually compiles.
3. **Run `cargo check --workspace --all-features`** (or the relevant
   subset for the project — for this repo, `--features
   remanence-cli/linux-udev` enables the udev backend). Resolve every
   error. Each compiler error is a design-level warning — silencing
   it without understanding has cost us real rework.
4. **Then** write the markdown design doc, referencing the stub as
   the verified source-of-truth.
5. **Decide**: either keep the stub as the start of the implementation
   (commit it), or delete it (mark as design-only). Don't leave
   half-cooked stubs lying in the worktree.

The 30-minute investment up front replaces a two-round codex cycle on
the back end. Net win on every previous design where this was skipped.

## The five recurring categories

Each of these has bitten the project at least once. The skill's job is
to make me check for each one explicitly before declaring a design
ready.

### Category 1 — Module privacy direction

**Rule:** Rust grants visibility **down** the module tree, not
laterally. A child module sees everything in its parent (including
`private` fields). A sibling does NOT.

```
src/
  handle/
    mod.rs       ← LibraryHandle { is_dirty: bool }   (private)
    tape_io.rs   ← can see is_dirty (CHILD of handle)
  other.rs       ← CANNOT see is_dirty (SIBLING of handle)
```

**How this bit us:** Layer 3a design v0.1 proposed
`crates/remanence-tape`; codex `93f242da` caught it. v0.2 proposed
`src/tape_io/` as a sibling of `src/handle/`; codex `97997d71`
caught that. v0.3 is the first version with `tape_io` as a child of
`handle`.

**Pre-design check:** for every new module accessing private fields of
an existing type, **trace the module path from the new module to the
type's defining module**. The new module must be a descendant of (or
equal to) the defining module. If it isn't, either restructure or
widen the field's visibility (`pub(crate)`, `pub(super)`).

### Category 2 — `!Send` types in threading + async

**Rule:** Types holding raw FFI pointers (libudev, anything from
`*-sys` crates) are usually `!Send`. `tokio::spawn` requires the
future to be `Send`. `std::thread::Builder::spawn` requires the
closure to be `Send`. `!Send` values cannot be moved across either
boundary.

**Solutions:**

- **Build the `!Send` value inside the destination thread.** The
  thread's closure constructs it; no movement happens. This is what
  `LinuxUdevSource::subscribe` does.
- **Use `tokio::task::spawn_local` + a `LocalSet`.** Single-threaded
  tokio context; permits `!Send` futures. Adds caller burden (every
  consumer must set up a LocalSet).
- **Wrap in `Arc<Mutex<...>>` + send the `Arc`.** Only safe if the
  underlying C library is itself thread-safe (libudev contexts are
  not — this option does NOT apply for tokio-udev).

**Pre-design check:** for every type the design moves across a thread
or `tokio::spawn` boundary, **search the source crate for `*mut`**
(raw pointers) or `impl !Send`. If found, the type is `!Send`; design
must build it on the destination thread.

**How this bit us:** Layer 2c design v0.1 said "tokio::spawn the
coalescing loop." Codex `411a4e8b` flagged it (design-only). I
implemented anyway; live build surfaced the same error twice — once
via `tokio::spawn` failing, then via `std::thread::Builder::spawn`
also failing (closure move of `!Send`). The actually-shipped fix
builds the monitor inside the dedicated watcher thread.

### Category 3 — Reactor-registration timing

**Rule:** tokio-aware wrappers that hold an `fd` register with the
**current reactor at construction time**. `AsyncFd`, `AsyncMonitorSocket`
(tokio-udev), `TcpStream::from_std`, `UnixStream`, etc. all need an
active runtime when constructed, not just when polled.

This means:

```rust
// ❌ Wrong — no reactor at construction time.
std::thread::spawn(|| {
    let monitor = AsyncMonitorSocket::try_from(sync_socket)?;  // panic!
    let rt = Runtime::new()?;
    rt.block_on(use_monitor(monitor));
});

// ✓ Right — construction inside block_on.
std::thread::spawn(|| {
    let rt = Runtime::new()?;
    rt.block_on(async move {
        let monitor = AsyncMonitorSocket::try_from(sync_socket)?;  // ok
        use_monitor(monitor).await;
    });
});
```

**Pre-design check:** for every tokio-aware type the design constructs,
read its `new()` / `try_from()` rustdoc. If it mentions "current
reactor," "tokio runtime," or "must be called from within a Tokio
runtime," the construction must be inside `block_on` or a tokio task,
not before.

**How this bit us:** Layer 2c live smoke surfaced a runtime panic
("there is no reactor running") after the design + code were both
otherwise correct. The fix was a 5-line restructure of the spawned
closure.

### Category 4 — Borrowed-handle plumbing

**Rule:** A struct holding `&'a mut T` exclusively borrows from `T`.
You **cannot** also hold `&'a mut U` from the same parent if `U` and
`T` live in the same struct, unless you split-borrow.

```rust
// ❌ Wrong — two mutable borrows of `self`.
fn open_child(&mut self) -> Child<'_> {
    Child {
        a: &mut self.field_a,
        b: &mut self.field_b,   // error: cannot borrow `self.field_b` as mutable...
    }                            // because `self` is already mutably borrowed
}

// ✓ Right — factor into a struct, borrow the struct.
struct Inner { a: A, b: B }
fn open_child(&mut self) -> Child<'_> {
    Child { inner: &mut self.inner }
}

// ✓ Also right — split-borrow into the fields directly (one expression).
fn open_child(&mut self) -> Child<'_> {
    let Self { field_a, field_b, .. } = self;
    Child { a: field_a, b: field_b }
}
```

**Pre-design check:** if a child handle borrows more than one field of
its parent, either (a) factor those fields into a single nested struct
that the child borrows whole, or (b) use a destructuring split-borrow
in the constructor. Don't promise the design works without writing the
constructor.

**How this bit us:** Layer 3a design v0.2 proposed extending
`DriveHandle` with a `&'a mut DirtyState` borrow alongside the existing
`audit_hook: &'a mut Option<AuditHook>`. The two are separate fields
of `LibraryHandle`, so the constructor would need a split-borrow.
Codex `97997d71` flagged it; v0.3 explicitly factors `DirtyState` as
a struct *first*, then borrows it.

### Category 5 — Trait/method visibility traps

**Rule:** A `pub fn` on a type whose internal state is private is
still callable — but only if the *caller* has a value of that type.
External crates that don't have a constructor for the type can't call
its methods at all.

Worse: traits implemented for a type can be `pub`, but if the trait
methods need to manipulate private state, the impl lives in the
defining crate. An external crate cannot add a useful impl from the
outside (orphan rules).

**Pre-design check:** for every public API the design proposes,
**trace the caller path**:

1. Where does the caller construct the type? Is the constructor
   `pub`?
2. What private state does the method need? Is the impl in the same
   crate as the state?
3. If the API spans crates, what `pub(crate)` widening is required?
   Document it explicitly.

**How this bit us:** Layer 3a design v0.0 (the original spec line)
called for `crates/remanence-tape` containing `TapeIoHandle<'a>::
from_drive(drive: DriveHandle<'a>)`. The wrapped methods needed to
issue CDBs through `DriveHandle::transport` (private field) and emit
audit on `DriveHandle::audit_hook` (private field). Codex `93f242da`:
"no public capability exists for an external crate to issue arbitrary
CDBs through the drive." Fix: bring the impl into the same crate.

## The verification protocol

For each design doc, before committing:

1. **Run the five-point checklist above.** Note any matches in the
   doc itself — usually in §2 or a "design notes" section — so future
   readers see what was checked.
2. **Write a compiling skeleton.** Stub every new struct, trait, and
   impl in the actual target crate. Use `unimplemented!()` or
   `todo!()` for bodies. Use `#[allow(dead_code)]` to silence
   warnings.
3. **Run `cargo check --workspace --all-features`.** (Substitute the
   project-relevant feature set; in this project it's `--features
   remanence-cli/linux-udev`.) **Resolve every error.** Each error
   that is silenced without understanding is a design bug.
4. **Run `cargo clippy --workspace --all-targets -- -D warnings`**
   for good measure. Catches `dyn` trait object issues, `Send` bound
   problems, and lifetime elision surprises.
5. **Decide on the stub's fate**:
   - If implementing immediately: commit the stub as the first step
     of the implementation plan (becomes Step N.0 in the doc).
   - If design-only: delete the stub. The compile-check happened; the
     binary doesn't need it.

**Document in the design doc what was checked.** A brief "verified
against compile-check on <date>" line in §2 or §10 of the doc tells
codex (and future-me) that the design isn't just paper.

## When to skip the verification

The full protocol is overkill if:

- The design touches no new Rust types — e.g., a doc that only
  re-states existing API surface, or a positioning piece like
  `why-remanence.md`.
- The change is mechanical (renaming a field, deleting a method).
  The implementation cycle will catch any errors directly.
- The proposal is intentionally provisional and will be reworked
  after a stakeholder discussion. (But in that case, mark the doc
  as draft.)

Default is **don't skip**. The cost of running the protocol is low;
the cost of skipping has been demonstrated.

## Project history this skill draws from

Concrete cases logged in `journal/2026-05-18.json`:

- Codex `411a4e8b` (Layer 2c design, pre-impl): caught `!Send` issue
  + send/try_send slow-consumer issue + `has_unknown_scope` gap. I
  proceeded with implementation anyway because I didn't poll the
  journal. Two of three findings then re-surfaced as compile errors;
  the third was caught by codex's next review cycle.
- Codex `93f242da` (Layer 3a design v0.1): caught the
  `remanence-tape` crate-cannot-access-private-fields problem.
- Codex `97997d71` (Layer 3a design v0.2): caught the
  sibling-module-also-cannot-access-private-fields problem AND the
  DriveHandle-doesn't-carry-DirtyState problem.
- Codex `dd58abe9` (this skill, v0.1): caught the
  `#[cfg(any())]` false-pass trap in the skill's own first draft —
  the skill author listing an always-false cfg as an acceptable
  stub annotation, which would have produced exactly the "skeleton
  passes cargo check but isn't actually compiled" outcome the skill
  is meant to prevent. Concrete demonstration that the discipline
  is needed even by people who wrote the discipline.

All of these would have been caught by `cargo check` of a skeleton in
under five minutes. The skill exists to make that the default.

## Output expectations

When this skill fires, the design doc you write should:

1. Reference the skeleton (file path) used to verify the design.
2. Include a line near §2 or §10 saying "verified against
   `cargo check --workspace --all-features` on <date>; skeleton at
   `<path>`."
3. Address each of the five categories in the design doc body —
   either by passing the check naturally, or by explicitly noting
   the constraint and the resolution.

This makes the verification part of the audit trail, not just an
implicit assumption.
