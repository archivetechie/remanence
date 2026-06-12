# Multi-drive concurrency architecture (Layer 2 + Layer 5) v0.1

Status: agreed design direction (Claude + owner, 2026-06-04), prompted by the
2026 architectural review (`docs/architectural-review-2026.md`, Gemini), which
flagged the **global hardware lock** as the primary pre-1.0 architectural debt.
This doc is the umbrella; each **phase** below is implemented as its own
design → verify → spec → plan → Codex cycle. Phase 1's Rust types are
compile-verified (see §Verification); later phases are design-direction, to be
detailed per-phase.

## Problem

All hardware access — reads, writes, robotics, reconcile — funnels through a
single `write_owner` thread, serialized by one global `session_busy` flag. A
10-hour read on drive 1 makes the **entire** multi-drive library report busy: a
`LoadDrive` or write to drive 2 returns `FAILED_PRECONDITION`. An MSL3040 with
four LTO-9 drives is reduced to a one-drive bottleneck in software.

## Core insight: two concurrency classes, conflated today

The hardware has **two** resource classes with **different** concurrency:

- **The changer is serial.** One robot arm; one MOVE MEDIUM at a time. This
  *must* serialize. (Real hardware law.)
- **The drives are parallel.** Each LTO drive is an independent SCSI target on
  its own device node (`/dev/tape/by-id/…`, its own transport/fd). Reading drive
  1 while writing drive 2 is physically supported. (Confirmed: drives + changer
  each have distinct `by-id` entries → independently addressable targets; the
  changer is its **own** target, not a LUN behind a drive.)

Today's single lock serializes **both**, not because the drives require it, but
because of one narrow software coupling: `DriveHandle<'a>` borrows
`&'a mut audit_hook` and `&'a mut dirty` from `LibraryHandle`. That borrow — over
two small fields — is the *only* thing preventing two drives from being open at
once. It does **not** borrow the changer transport (each drive opens its own) and
does **not** borrow the inventory snapshot (it carries a clone). The bottleneck
is a Rust-borrow/software-structure choice, not a hardware necessity.

**The architecture stops conflating the two: model each physical resource as its
own actor, owning its own `!Send` hardware handle + catalog connection in its own
thread.**

## Target architecture — per-library supervisor tree

```
        async gRPC layer  ── thin `mount` orchestration module (async fns) ──┐
                 │ mpsc commands + oneshot replies (Send data only)          │
   ┌─────────────┼──────────────────────────────┐                           │
   ▼             ▼              ▼                 ▼          reservation +    │
ChangerActor  DriveActor[0]  DriveActor[1] ...  DriveActor[N-1]  session-map │
(1 thread)    (1 thread)     (1 thread)         (1 thread)   live in ApiState┘
 changer dev   drive0 dev     drive1 dev         driveN dev
 inventory     own catalog    own catalog        own catalog
 own catalog   conn           conn               conn
 conn
```

- **1 ChangerActor** per logical library: owns the changer transport + the
  authoritative inventory; does MOVE / READ ELEMENT STATUS / PREVENT-ALLOW and
  the changer half of load/unload. Serializes the robot. Single writer of the
  S6b `Arc<RwLock<Arc<LibrarySnapshot>>>` inventory cell → consistent reads.
- **N DriveActors** (one per physical drive bay): each owns its drive transport
  and does SSC LOAD/UNLOAD + READ/WRITE/LOCATE/SPACE/verify/reconcile for *that*
  drive. They run **in parallel** — separate threads, separate fds.
- Each actor builds its own `!Send` resources **in its own thread** (transport,
  `rusqlite::Connection`) — the trick the current single owner already uses,
  replicated N+1 times. No shared `!Send` state; no locks around hardware.

Why this shape (vs. alternatives): a single thread cannot overlap blocking
`SG_IO` ioctls, so OS threads per drive are *required* for real parallelism (rules
out "one smarter owner"). Persistent thread-*per-drive* beats ephemeral
thread-*per-session* because the drive is a durable resource with state (loaded
tape, `DRIVE_STATUS_BUSY`, dirty) that wants a stable home — and it isolates
failure (one drive's panic restarts just that actor).

## Orchestration boundary — thin in-daemon `mount` module (the "A-synthesis")

Mount/unmount spans two resources:

```
open: read inventory → reserve a free bay B → CHANGER MOVE slot→B
      → DRIVE[B] SSC LOAD + verify identity → stream
close: DRIVE[B] finalize(EOD) + SSC UNLOAD → CHANGER MOVE B→slot → release B
```

The sequencing lives in a **dedicated `async fn` orchestration module** in the
daemon (`mount::open_session(...)`, `mount::close_session(...)`), driven by the
Tokio reactor (it `.await`s the ChangerActor then the DriveActor). The actors
stay dumb workers (no actor↔actor channels, no deadlock surface). Small shared
state — per-bay reservation flags + a `session_id → bay` map — lives in
`ApiState` behind atomics / a short-held `Mutex`.

This is the reactor's *mechanism* (no chokepoint: the only serial point is the
ChangerActor, held just for the brief MOVE) with a coordinator's *organization*
(one tested, reusable place for the choreography + cross-resource invariants).
If scheduling/recovery later grows teeth, the module already defines the protocol
and can be promoted to a coordinator actor without touching handlers or workers.

### Layer boundary: why this is remanence's job, and why it stays thin

Arbitration of a shared resource must live **at** the resource. Drive-pool
allocation + changer serialization + per-mount hardware-safety invariants
("don't MOVE into an occupied bay," "don't swing the robot while that bay's drive
streams," "this drive is reserved") cannot live in sutradhara: there may be >1
consumer (a second job, `rem-debug`, the harness), and hardware safety must not
depend on client correctness (defense-in-depth, like the existing AccessPolicy
allowlist). Remanence owning the session lifecycle also gives it the durable
audit/operation record for restart reconciliation.

But remanence does only the **mechanism**: drive-pool *admission* (is a drive
free? reserve it), changer serialization, the safe single-session sequence,
cleanup on failure. **sutradhara owns the policy** — which objects, which pool,
what priority/order, multi-copy placement, when to scrub — i.e. *what runs and
when*. Remanence answers "all drives busy → `FAILED_PRECONDITION`"; sutradhara
schedules. Because the scheduling intelligence (which would justify a heavy
coordinator) is upstream, remanence's orchestrator stays a thin module. The raw
`MoveMedium`/`LoadDrive`/`UnloadDrive` primitives remain exposed as a manual
escape hatch; the safe default (`OpenWriteSession{pool}`) orchestrates internally.

## Hardware topology — addressing vs. physical path

Independent *addressing* (distinct `by-id` targets) ≠ independent *physical
path*. All targets ride one SAS link routed **through the bottom drive's
connector** + the library's internal expander. Consequences:

- **Concurrency correctness: unaffected.** Distinct targets → the kernel SCSI
  mid-layer queues per target; concurrent commands to different drives are
  correct. The actor-per-drive model holds.
- **Throughput: capped by the shared path, not N×.** This work delivers *N
  concurrent independent sessions / overlapped ops*, not guaranteed N× linear
  bandwidth. (Set expectations; doesn't change the design.)
- **Failure: drives are NOT independent failure domains.** The bottom/master
  drive is a privileged shared dependency — lose it (pulled / failed / slot
  empty) and the changer **and every drive** go dark at once.

## Failure & supervision model (library-rooted)

The supervisor distinguishes two failure classes:

- **Isolated drive-actor fault** (one drive's ioctl errors / panic) → restart
  just that actor; the others keep streaming. Fixes Gemini §D (blast radius)
  *better* than today — failure is now per-drive, not whole-daemon.
- **Library-path loss** (the shared path is gone — the bottom-drive `by-id`
  targets stop resolving en masse) → mark the whole library unreachable, quiesce
  all actors, attempt re-discovery; do **not** thrash-restart actors against a
  dead bus.

Liveness is rooted at the **library/changer path** (the ChangerActor's
reachability), not per-drive. Each actor pins its device by **stable identity**
(`/dev/tape/by-id` symlink, or the VPD-0x80 serial revalidation Layer 2 already
does), never the ephemeral `/dev/sgN` — so "DriveActor for serial X" reliably
owns the same physical drive across rescans, and the en-masse by-id resolution
failure is the path-loss signal.

## Catalog under concurrency (not a bottleneck)

`rusqlite::Connection` is `!Send`, so each actor keeps **its own** connection
(the read path already opens `open_read_only` per call). Enable **WAL** so
readers never block and writers serialize at the SQLite level. The heavy work
(tape I/O) takes hours and parallelizes; the per-object index update takes
milliseconds and serializing those is negligible. (Confirm WAL is enabled.)

## Cancellation (Gemini §B)

With per-drive actors, cooperative cancellation moves *into* each drive's I/O
loop: a cancel token checked between block writes lets a long write abort
gracefully — flush a final EOD filemark, leave the tape consistent, release the
drive — instead of "before-dispatch only." This is a Phase-4 concern but the
actor model is what makes it clean (each drive owns its own abortable loop).

## Phases (each its own cycle)

1. **Layer 2 — own the `DriveHandle`.** Drop the `&'a mut audit_hook`/`dirty`
   borrows: `DriveHandle` owns its audit sink + its own `DirtyState`; add an
   owned constructor so a drive opens **without** a live borrow of the
   changer/library. No behavior change yet (single owner still works) — purely
   lifts the lifetime constraint. **Smallest, highest-leverage, independently
   verifiable. Armed first.**
2. **Layer 2 — extract `ChangerHandle`** from `LibraryHandle`: the changer
   transport + inventory authority + changer-side dirty/audit, separated from the
   drive concerns. Composed load/unload become a changer-step + drive-step pair.
3. **Layer 5 — the supervisor tree.** ChangerActor + N DriveActors; per-drive
   reservation replacing the global `session_busy`; the thin `mount` module;
   per-actor catalog connections + WAL; `DRIVE_STATUS_BUSY` from live per-drive
   state. **Concurrency turns on here.**
4. **Resilience.** Mid-stream cooperative cancellation (§Cancellation); the
   library-rooted supervisor with per-actor restart vs. path-loss handling
   (§Failure).

## §Verification

Phase 1's types (owned `DriveHandle` with `audit_hook: Option<AuditHook>` +
`dirty: DirtyState` by value, owned constructor, two coexisting `DriveHandle`s)
are compile-verified in the Phase 1 spec/plan. Later phases are design-direction;
their Rust types are verified when each is specced. The five rust-design-
verification categories most in play across this work: **!Send in threading**
(per-thread transport + `rusqlite::Connection`, Send-only messages),
**borrowed-handle plumbing** (the whole point of Phase 1), and **reactor timing**
(actors built inside their threads; orchestration on the reactor).
