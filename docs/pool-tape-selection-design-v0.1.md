# Pool Tape-Selection Policy v0.1

Status: design draft. Normative Layer 5 surface remains `docs/spec-v0.4.md` §11.4
and `proto/layer5.proto`; this document specifies the behaviour behind
`TapePoolTarget` and the write-session tape lifecycle that the spec currently
leaves open.

Companion document: Sutradhara's `docs/tape-write-policy-v0.1.md` covers the
policy responsibilities that deliberately do **not** live in Remanence
(priority, copy-set completion ordering, drive reservation, backpressure). Read
the two together; the boundary between them is the load-bearing decision here.

---

## 1. Problem

`OpenWriteSession` accepts a `TapePoolTarget` (`proto/layer5.proto:745`) meaning
"daemon, choose and mount an eligible tape from this pool." The current
implementation is a placeholder: the reusable selector `select_tape_in_pool`
(`crates/remanence-api/src/pool_write.rs`, eligibility in
`phase1_tape_is_eligible`) returns the pool's single eligible tape and otherwise
errors `AmbiguousNeedsPolicy` — explicitly deferring multi-tape choice to this
policy workstream. (The `select_write_target` `PoolTarget` branch in `lib.rs` is
a parallel stub that grabs the first empty tape.) That is not a policy — it
cannot fill a tape across sessions, cannot pack objects efficiently, and cannot
decide when a tape is "done." Code references here are by symbol name rather than
line number, which drifts.

Sutradhara already owns the question of *which pool* a copy belongs to — it maps
copy/content intent to a `pool_id` and Remanence only enforces it
(`spec-v0.4.md:112`, `:1225`). What is undefined is everything *within* a pool:
given a `pool_id`, which physical cartridge is mounted and written, when a tape
rolls to the next, and when a tape is sealed.

## 2. Scope

**Remanence owns mechanism, not policy.** Concretely, in scope:

1. **Within-pool tape selection** — a pluggable policy with one shipped default.
2. **Spanning write sessions** — a write session targets a *pool* and may write
   an ordered sequence of tapes, not just one.
3. **Drive/mount selection** — prefer an already-loaded eligible tape; otherwise
   take a free drive; fail cleanly when none is free.
4. **Concurrency** — concurrent sessions on the same pool must never be handed
   the same tape.
5. **State exposure** — surface the inventory/occupancy facts Sutradhara needs to
   schedule.

Explicitly **out of scope** (Sutradhara's job, documented in the companion doc and
named here only so the boundary is unambiguous): job priority, restore-vs-ingest
ordering, copy-set completion ordering, work-in-progress caps, staging
backpressure, drive reservation / production-hours scheduling, and cooperative
yielding of a drive. Remanence provides the levers; Sutradhara pulls them.

The selection-policy Rust surface (§12) is compile-verified against `cargo check
--workspace` on 2026-05-29; skeleton at
`crates/remanence-api/src/pool_selection.rs`.

## 3. Concepts

- **Active vs sealed tape.** An *active* tape may still be appended to. A
  *sealed* (finalized) tape is closed to writes and ready to be shelved.
- **Usable capacity / high watermark.** Per pool, `watermark_high ∈ (0,1]` is a
  fraction of raw cartridge capacity. Nothing is ever written past
  `capacity × watermark_high`. It is a soft logical cap that reserves headroom
  below physical end-of-media; the hardware EOM/early-warning flags
  (`spec-v0.4.md:834`) are the backstop beneath it.
- **Low watermark / fill target.** `watermark_low ∈ (0, watermark_high)`. When a
  write pushes a tape's `used` across `capacity × watermark_low`, the tape
  becomes complete and is sealed (see §4). In practice `low` behaves as the
  *fill target*, not merely an "eligible to seal" hint.
- **Projected footprint.** The space an object will actually consume, including
  its filemark, parity sidecars, bootstrap blocks, and reserve. Remanence
  already computes this precisely in the no-spanning preflight
  (`spec-v0.4.md:1061`); selection reuses that computation rather than a flat
  percentage. Where the caller supplies `declared_size_bytes`
  (`proto/layer5.proto:765`) it seeds the estimate; otherwise the daemon
  estimates.
- **`S_min`.** The smallest object the workload will hand Remanence — in
  practice Sutradhara's minimum bundle size. Recorded per pool as
  `min_object_size` (see §6). Objects never span tapes (`spec-v0.4.md:1061`), so
  a single object larger than an empty tape's usable capacity is rejected
  outright, unchanged by this design.

## 4. Default selection policy: "complete-or-fill"

Selection runs **per object**, at each object boundary within a session (the
daemon owns this loop — §5). The session has a **current active tape** `T_cur`,
the one mounted and being written. `P` is the projected footprint (§3) of the
next object. The first rule is stickiness, to avoid robotic thrash:

```
# Tier 0 — stick to the mounted tape (no robotic move)
if T_cur exists and (usable(T_cur) - used(T_cur)) >= P:
    write object to T_cur                 # seal decided after the write (§4.1)
    done

# Otherwise ROLLOVER (§5): T_cur cannot take this object; choose a new tape.
candidates = active tapes T in pool where (usable(T) - used(T)) >= P
             and T is not reserved by another live session   (see §7)

# Tier 1 — complete a tape
seal_set = { T in candidates : used(T) + P >= low(T) }   # reaches or crosses low
if seal_set not empty:
    choose T* = argmin over seal_set of (usable(T) - used(T) - P)   # best fit
                tie-break: already-loaded in a free drive, then lowest barcode

# Tier 2 — fill the oldest open tape
elif candidates not empty:
    choose T* = an already-loaded candidate if any, else lowest barcode

# Tier 3 — need a fresh tape
else:
    promote an unassigned blank tape in the pool to active, or
    fail the append with `PoolExhausted` (§13)

mount T* into the drive the session already holds (§8); write the object;
decide the seal on actual position (§4.1)
```

Stickiness (Tier 0) comes first because without it a `[Huge, Small, Huge,
Small]` stream would re-evaluate the pool for every object and, by
lowest-barcode ordering, bounce the robot between tapes on every small object.
Sticking to the mounted tape whenever the object fits collapses that to the
mounts genuinely forced by an object that does not fit `T_cur`.

The tiers encode the behaviour worked out from d2's `VolumeUtil`, now applied
only **at rollover** — when a new tape must be chosen anyway, so the mount it
implies is already unavoidable:

- **Tier 2 is d2's rule** (`getToBeUsedPhysicalVolume`): fill the lowest-barcode
  open tape that fits. A big object that does not fit a tape does **not** finalize
  it — the tape stays open and is revisited at a later rollover, while meanwhile
  the *current* tape keeps absorbing objects via Tier 0. Among equally-good
  candidates an already-loaded tape wins, to avoid a mount.
- **Tier 1 is the refinement over d2**: when several open tapes could take the
  object, prefer the one this write *completes* (pushes past `low`), packed
  tightly (best fit) to minimise the wasted tail before sealing. Example: object
  45 GB, tape A has 100 GB usable-free, tape B has 50 GB — both fit, but only B
  is completed, so route to B and seal it. A Tier-1 mount is rare (once per tape,
  at its sealing) and so worth the robotic move; the per-object churn that Tier 0
  prevents is the costly kind.

### 4.1 Eager sealing — decided on actual position

The seal decision is taken **after** each write, from the tape's actual
post-write position (`BodyPosition` / EOM early-warning, `spec-v0.4.md:834`), not
from the projected estimate used to *select* the tape. When real `used` reaches
or crosses `low` (`used >= low`, or hardware early-warning fires first), the tape
is sealed immediately, regardless of which tier selected it. The boundary is
inclusive on purpose: a write landing exactly on `low` seals, so every *open*
tape strictly satisfies `used < low` — which is what §6's invariant relies on. Deciding on reality rather than the
projection closes the gap where an under-projected footprint pushes a tape past
`low` without Tier 1 ever routing it there — otherwise it would sit open above
`low`, unable to satisfy Tier 1 again, until the force-seal valve (§4.2) caught
it.

Rationale for eager sealing: it minimises the number of open tapes in a pool and
gets cartridges to the shelf sooner. To recover the band between `low` and
`high`, the operator raises `watermark_low` (§6); we do not keep above-`low`
tapes open for further top-ups.

Consequence to hold in mind: an *open* tape therefore always has actual
`used < low`, so its usable-free is always strictly greater than the band width
`(high − low) × capacity`. This is what makes the invariant in §6 exact.

### 4.2 Force-seal valve

Eager sealing guarantees no tape with sub-band free space stays open — but it
cannot guarantee a correctly-sized object ever *arrives*. A tape can legitimately
sit open below `low` because the workload only sent objects too large for its
remaining space. That tape is not stranded (a smaller object could still land),
but it may idle indefinitely.

The force-seal valve seals a below-`low` tape when it is clearly done:

- it has been passed over because no pending/incoming object fits its remaining
  usable space, **or**
- Sutradhara/the operator explicitly requests close-out of the pool or the tape.

Without this valve, an unusual object-size distribution leaves capacity open
forever. With §3's eager sealing removing natural top-up of above-`low` tapes,
the valve is the primary mechanism that closes out idling tapes — not a
belt-and-suspenders extra.

## 5. Spanning write sessions

This changes the session model in `spec-v0.4.md:1835` ("bound to one drive, one
loaded tape, one body format"). A write session opened with a `TapePoolTarget`
binds to a **pool**, not a tape, and may write an **ordered sequence of tapes**
over its lifetime.

A session is a live, stateful, recoverable write context — **not a queue of
objects**. The orchestrator streams objects into it one at a time
(`AppendObjectStart`→`Chunk`→`Finish`, `proto:751`); the session has no
foreknowledge of what or how many objects are coming. Object ordering, batching,
and priority live entirely in Sutradhara (companion doc), never in the session. The
only forward-looking hint the session sees is the optional per-object
`declared_size_bytes`, which seeds `P` for the current placement.

**Rollover is a between-objects, session-level event. Objects never span a tape**
(`spec-v0.4.md:1061`); only the session spans tapes. Concretely:

1. The session is writing tape B. Object N completes **wholly** on B — data,
   filemark, sidecars.
2. Object N+1 arrives. Selection (§4) finds it does not fit B's remaining usable
   space (Tier 0 misses).
3. The daemon seals or leaves B per §4, selects the next tape C, **unloads B and
   loads C into the drive the session already holds** (§8), and writes object N+1
   **wholly** onto C. C becomes `T_cur`.

Because rollover reuses the session's *own* drive (unload-in-place, or a switch
to another free drive that already holds C), it never needs to acquire a *free*
drive and **cannot fail with `NoDriveAvailable`** — that failure is only possible
at the initial `OpenWriteSession` (§8). A `DriveTarget` / `TapeTarget` session is
unchanged: pinned to the one named tape, a tape-full condition ends it rather
than rolling over.

**Recovery / orphaned sessions.** The session's audit and journal state must
record the *sequence* of tapes and the object→tape placement, not a single
`tape_uuid`. On daemon restart, an `ORPHANED` pool session
(`spec-v0.4.md:1837`) is recoverable to its last committed object on its last
tape; resume continues selection from there. The write-session schema and
`WriteSession` message grow from a single tape reference to an ordered
tape list. (This is the largest data-model change in the design and the part the
implementation plan should sequence first.)

## 6. Watermarks and the `band ≥ S_min` invariant

Per-pool configuration carries `watermark_low`, `watermark_high`, and
`min_object_size`. The load-bearing invariant:

> **`(watermark_high − watermark_low) × capacity ≥ min_object_size`**

If the band is narrower than the smallest object, a tape can glide into the dead
zone just under `low` where nothing fits and nothing seals it — permanently
stranded with wasted, unsealable capacity. Proof sketch: permanent stranding
needs `usable_free < S_min` (nothing fits) **and** `used < low` (never sealed);
substituting `usable_free = high − used` gives `high − S_min < used < low`, which
is only satisfiable when `high − low < S_min`. Make the band at least one object
wide and the dead zone closes: any tape whose free space drops below `S_min` has
already crossed `low` and been sealed.

Enforcement: the daemon **validates this at config load** and rejects/warns when
it fails (joining the existing config validation in `spec-v0.4.md:1221`).
`min_object_size` is the operator's declaration of Sutradhara's bundle floor; with
no guaranteed floor it is set to 0 and the force-seal valve (§4.2) becomes the
only stranding defence.

**Band sizing — keep it small.** The invariant lower-bounds the band, but eager
sealing (§4.1) pushes the other way: because a tape is sealed the moment actual
`used` crosses `low`, the band `(high − low)` is *wasted headroom*, not usable
space — the most a tape can strand at seal time is one band width. So set the
band only a little above `min_object_size`, with a margin for footprint
projection error, and set `low` as high as the hardware reserve below `high`
allows. A wide band is a mistake here: low 0.92 / high 0.97 on an 18 TB LTO-9
would strand up to ~0.9 TB *per tape*. A tight band such as low 0.965 / high 0.97
(~90 GB) clears a 2 GB bundle floor with ample projection margin while wasting at
most ~90 GB; tighter is better so long as the invariant and the projection margin
still hold. The config-load check guards the *lower* bound (band narrower than
`S_min` strands tapes); avoiding a wastefully *wide* band is operator judgement,
and the doc states the rule so that judgement is informed.

**Assumption stated, not relied on:** Sutradhara emits logical assets at or above
its bundle floor, so `S_min` is a true hard floor. The one exception is an
end-of-source runt bundle below the floor; Remanence's design does not depend on
either case — the invariant plus the valve handle any object size — but the doc
records the assumption so it is not silently load-bearing.

## 7. Concurrency

Multiple sessions may target the same pool. Selection (§4) must therefore exclude
tapes that another live session has already been handed, and the chosen tape is
**reserved** to the session for as long as it is its active tape. A tape reserved
by a live session is invisible to other sessions' selection until released
(sealed, rolled past, or the session closes/aborts/orphans). This prevents two
sessions from being mounted onto the same cartridge and prevents double-counting
of `used` during placement decisions. Reservation is daemon-local runtime state,
released on the same authority as the per-library flock (kernel lock / session
teardown), not durable on-tape authority.

**Crash recovery ordering.** Because reservation is daemon-local, a restart wipes
it — which would let a restarted daemon hand a *new* session a tape that a
recoverable (`ORPHANED`) session is mid-write on, before Sutradhara gets around to
resuming it. To prevent that, bootstrap must **rebuild reservations for all
recoverable sessions from durable journal/audit state before it begins accepting
`OpenWriteSession` calls** (sequenced into the daemon's existing startup
reconciliation, `daemon-runtime-v0.1.md`). A tape belonging to a recoverable
session is reserved from the first moment new sessions can be opened, not from
whenever resume happens.

## 8. Drive/mount selection

A separate, smaller mechanism layer from tape selection.

**At initial `OpenWriteSession`:**

1. If the selected tape is **already loaded** in a drive, use that drive — no
   robotic move. The dominant cost saver; always preferred.
2. Otherwise mount the selected tape into any **free** drive.
3. If no drive is free, **fail the open cleanly** before any data is written (the
   established pattern, `spec-v0.4.md:1823`). Remanence does not queue, preempt,
   or arbitrate between sessions for a drive — that is Sutradhara's scheduling
   decision (companion doc). Remanence only reports the facts.

**At rollover (§5):** the session already holds a drive, so it does **not** need
a free one. It unloads the current tape and loads the next into the **same
drive** — unless the next tape is already loaded in another free drive, in which
case it uses that drive and releases its own (pure mount-avoidance). Rollover
therefore never fails with `NoDriveAvailable`.

"Prefer the loaded tape" is purely an efficiency default. It carries no
scheduling judgement and cannot starve other work, because Remanence never holds
a drive against a competing session of its own accord — Sutradhara decides what
sessions exist and when to release them.

## 9. State exposure for Sutradhara

Sutradhara's scheduler (reservation, ordering, backpressure) needs facts only the
daemon has. Exposed via the existing `Catalog` / session surface:

- Drive inventory and per-drive occupancy: which drive holds which tape, idle vs
  busy, and the session/kind currently using it.
- Loaded-tape set per library (so Sutradhara can favour work that reuses a loaded
  tape, and reason about reservation headroom).
- Per-session progress and `declared_size` so Sutradhara can distinguish a "huge
  write" session from a small one.
- Per-pool tape states: active/sealed, `used`, usable capacity, and pool
  membership (`ListTapes(pool_id=…)`, `Tape.pool_id`, already present).

These are read-only projections; none of them grant Sutradhara authority over
on-tape state.

## 10. Pluggability

The default is "complete-or-fill" (§4). The selection step is a named, pluggable
policy per pool (`selection_policy` in config), mergerfs-inspired so future
policies are cheap to add without touching the session/rollover machinery:

| name | rule | status |
|------|------|--------|
| `complete-or-fill` | §4 two-tier; eager seal | default, v1 |
| `fill-oldest` | Tier 2 only (pure d2 first-fit by id) | v1, trivial fallback |
| `most-free` | pick the active tape with the most usable-free (spread) | future |
| `least-free` | best-fit only, no Tier-1 seal preference | future |

A policy is a pure function of (pool's active tapes + their `used`/capacity,
projected footprint, watermarks) → selected tape or "need fresh." It does not
touch hardware or sessions; that keeps each policy independently testable.

Tier 0 stickiness (§4) is part of the session/rollover machinery, **not** the
policy: the policy is consulted only at rollover, to choose the *next* tape. So a
policy never reasons about the mounted tape or mount cost — those concerns stay
in the session layer, identical across all policies.

## 11. Configuration

Per-pool, extending the `[[tape_pools]]` config (`spec-v0.4.md:1196`):

```toml
[[tape_pools]]
pool_id          = "camera.copy-a"
selection_policy = "complete-or-fill"   # default if omitted
watermark_low    = 0.92                 # fill target
watermark_high   = 0.97                 # usable cap, below physical EOM
min_object_size  = "2GiB"               # Sutradhara bundle floor; 0 if none
```

Validation at load: `0 < low < high <= 1`; the §6 invariant; `selection_policy`
is a known name; existing pool-id/charset rules unchanged.

## 12. Rust API surface (compile-verified)

Verified against `cargo check --workspace` and `cargo clippy -p remanence-api
--all-targets -- -D warnings` on 2026-05-29; skeleton at
`crates/remanence-api/src/pool_selection.rs`. These are the shapes that compiled,
not a sketch. (Note the changes forced by §4.1: `Selection` carries no
`seal_after` — sealing is decided later from actual position — and
`already_loaded` / `low_bytes` are projected into `TapeFitState` so the policy
stays a pure value-function.)

```rust
/// Per-tape fit state, projected by the caller before policy is consulted.
/// `already_loaded` and the watermark-derived byte figures are projected facts,
/// keeping the policy free of catalog/hardware/session access.
pub struct TapeFitState {
    pub tape_uuid: [u8; 16],
    pub barcode_order: u64,
    pub already_loaded: bool,   // mounted in a free drive (mount-avoidance tie-break)
    pub used_bytes: u64,
    pub usable_bytes: u64,      // capacity * watermark_high
    pub low_bytes: u64,         // capacity * watermark_low
}

pub struct PoolSelectionContext<'a> {
    pub candidates: &'a [TapeFitState],   // pre-filtered: fits + not reserved (§7)
    pub projected_footprint: u64,         // P, incl. filemark/sidecars/reserve
}

pub enum Selection {
    UseTape { tape_uuid: [u8; 16] },
    NeedFreshTape,
}

pub trait PoolSelectionPolicy: Send + Sync {
    fn select(&self, ctx: &PoolSelectionContext<'_>) -> Selection;
    fn name(&self) -> &'static str;
}
```

The daemon holds the configured policy as `Arc<dyn PoolSelectionPolicy>` in
shared state; the skeleton compile-asserts that this trait object is object-safe
and `Send + Sync` (tonic async handlers require `Send`).

### 12.1 rust-design-verification checklist

The skill's five recurring categories, as checked against the skeleton:

1. **Module privacy** — `pool_selection` is a sibling of `pool_write` under
   `remanence-api/src/`; it imports only the `pub` `TapeUuid` and touches no
   private fields (the policy reads projected `TapeFitState` values, never
   `TapeRecord`). Passes by construction.
2. **`!Send` types** — no FFI pointers; `Arc<dyn PoolSelectionPolicy>` is
   compile-asserted `Send + Sync`, so it lives safely in `ApiState` across async
   handlers. Explicitly verified.
3. **Reactor-registration timing** — N/A; selection constructs no tokio-aware fd
   types, it is synchronous pure logic.
4. **Borrowed-handle plumbing** — `PoolSelectionContext<'a>` holds a single
   shared borrow; no multi-field split-borrow. The drive-reuse at rollover (§8)
   uses the *existing* `DriveHandle` borrow surface, not new API here. Passes.
5. **Trait/method visibility** — the trait is `pub` and object-safe (`&self`
   methods, no generics/`Self` returns); the resolver builds the trait objects in
   the defining crate. The `Arc<dyn …>` assertion also exercises object safety.
   Explicitly verified.

## 13. Error cases

- `PoolExhausted` — no active or promotable tape in the pool fits the object
  (Tier 3 fall-through). Surfaced at the append that triggered it; the session
  may be checkpointed so Sutradhara can add a tape and resume.
- `ObjectTooLargeForEmptyTape` — unchanged (`spec-v0.4.md:1061`); a single object
  exceeds an empty tape's usable capacity.
- `NoDriveAvailable` — at initial `OpenWriteSession`, selection succeeded but no
  drive is free to mount the chosen tape (§8). Not raised at rollover, which
  reuses the session's held drive.
- `TapePoolAssignmentConflict` — unchanged (`spec-v0.4.md:1830`); a tape carries
  committed copies from another/unknown pool and must not be silently reused.
- Config-load rejection when the §6 invariant or watermark ordering fails.

## 14. Testing plan

- Unit: each policy as a pure function over fixtured `TapeFitState` sets —
  Tier-1/Tier-2/Tier-3 transitions, best-fit tie-breaks, the crossing-`low`
  boundary, eager seal flag.
- Property: the §6 invariant holds ⇒ no fixtured object stream can leave a tape
  open with usable-free `< S_min`. Force-seal valve resolves the idle case.
- Integration (VTL on akash): a pool session that rolls across ≥2 tapes within
  one session; orphan + recover mid-roll; two concurrent sessions on one pool
  never selecting the same tape.
- Regression of the d2 scenarios: big-object-skip-then-top-up; complete-a-tape
  routing.

## 15. Open questions

1. **Fresh-tape promotion vs fail.** On Tier-3, does the daemon auto-promote an
   unassigned blank already in the pool's library to active, or always fail and
   let Sutradhara assign? Leaning: auto-promote only blanks that config/membership
   already place in the pool; never reassign a tape with foreign committed
   copies.
2. **Reservation granularity** under rollover: is the *next* tape reserved
   ahead of time (to let the robot pre-stage) or only at the moment of roll?
   v1: reserve at roll; pre-stage is a later optimisation.
3. **`barcode_order` source** when firmware omits a clean sequence — reuse the
   Layer 2 identity ordering, or a config-declared order per pool?
