# Tape Pool Membership: Barcode-Prefix Model v0.1

Status: design decision (rev 2 — added the uuid/barcode identity model and the
`relabel` damaged-label path). Supersedes the config-row membership model for
tape→pool assignment. Relates to `docs/pool-tape-selection-design-v0.1.md` (which consumes
membership via `list_tapes(pool_id)`), Phase-2 `2a-2` provisioning
(`system/docs/phase2-2a2-design.md`), and `docs/spec-v0.4.md` §11.4.2.

## Decision

A tape's pool membership is **derived from its barcode (voltag) via a small
`prefix → pool` rule table in config** — not stored as per-tape rows and not set
by a runtime command. Config declares pool *definitions* and the *prefix rules*;
membership is a pure function of `(voltag, rules)`, recomputed at reconcile.
A tape's pool is effectively **permanent** — set when the cartridge is barcoded;
routine relabeling does not happen. The rare exceptions (a damaged label, or a
genuine reassignment) go through a deliberate, **audited** `relabel` operation,
never a casual relabel.

## Problem this solves

Today membership is config-authoritative as explicit per-tape rows
(`tape_pool_memberships`), and reconcile is the owner: it deletes any membership
not present in config at daemon start (`remanence-state/src/index.rs` reconcile +
`delete from tape_pool_memberships`). Provisioning deliberately does **not** write
membership (`index.rs:504-506`: *"Provisioning owns only the `tapes` row. Pool
membership remains config/policy-owned"*). So `rem tape init --pool X` has nowhere
durable to record intent — anything it writes to the DB is reconciled away. The
result was a CLI flag that records intent only in stdout and a shell bridge
(`rem-init.sh`) to work around it.

Deriving membership from the barcode removes the problem at the root: there is no
per-tape membership state to persist, so nothing to wipe, no assignment event, no
audit machinery, no config-vs-DB precedence.

## The model

- Config gains an ordered `[[tape_pool_rules]]` table mapping a **barcode prefix**
  to a `pool_id`. Pool *definitions* (`[[tape_pools]]` with `copy_class` /
  `content_class`) are unchanged.
- A tape's pool = the `pool_id` of the matching rule for its `voltag`. Matching is
  **longest-prefix-wins** (so specific prefixes can override broader ones);
  reconcile rejects rules whose prefixes are ambiguous under that rule (e.g. two
  equal-length prefixes that could both match).
- Reconcile **computes** the membership projection from `rules × tape voltags`
  instead of reading hand-written membership rows. The downstream query/selection
  path (`list_tapes(pool_id)` join on `tape_pool_memberships`) is unchanged — the
  table is now a derived projection, never operator-edited.
- This mirrors an existing pattern: the codebase already derives the LTO
  generation from the voltag suffix (`lto_generation_from_voltag`, `…L9`). Pool is
  the same idea, one field over.

### Config shape (illustrative)

```toml
[[tape_pools]]
pool_id       = "camera.copy-a"
# copy_class / content_class as today

[[tape_pool_rules]]
prefix  = "ACM"          # barcode ACM…  -> camera copy-a
pool_id = "camera.copy-a"

[[tape_pool_rules]]
prefix  = "BCM"
pool_id = "camera.copy-b"
```

## Behaviour

- **`rem tape init <voltag>`** derives the pool from the voltag and **validates**
  it: a voltag matching no rule is rejected (or warned) at init, so an
  unrecognized label fails loudly rather than silently landing nowhere. The
  `--pool` flag is removed (it can't mean anything durable) — or kept only as an
  assertion that must equal the derived pool.
- **Selection / eligibility** are unchanged: they read the (now derived) pool
  membership exactly as before.
- **Unreadable or unmatched voltag** ⇒ the tape is in no pool ⇒ not eligible for
  pool-targeted writes. This is a safe, visible failure (the operator fixes the
  label); it never silently mis-assigns.
- **Pool is permanent.** A cartridge's barcode is immutable for all practical
  purposes, so its derived pool never changes in normal operation. The safety rule
  (`spec:1828` — a tape with committed copies must not be silently reassigned to a
  different/unknown pool) is enforced *for free*: the pool can't change because the
  barcode doesn't. The two deliberate exceptions — damaged-label replacement and
  (rarer still) genuine reassignment — go through the audited `relabel` operation
  below, never a silent edit.

## Tape identity: uuid vs barcode

Two identities live on every cartridge, with different roles:

- **`tape_uuid`** — a 16-byte Remanence-internal identity, generated once at first
  init, written into the **BOT bootstrap** (write-once), and used as the **catalog
  primary key** (`tapes.tape_uuid`, `object_copies.tape_uuid`). The operator never
  sees it. It is the stable anchor that survives a label change and guarantees a
  unique key even if barcodes are ever reused or duplicated.
- **voltag (barcode)** — the immutable, human-facing label on the shell, read by
  the library, and the **pool determinant** via the prefix rules.

Re-init (running init on a tape that already has identity) matches by **uuid**,
read from BOT:

| BOT uuid | barcode | behaviour |
|---|---|---|
| matches a catalog row | matches | idempotent **no-op** |
| matches a catalog row | differs, **with** a recorded `relabel` | accept (catalog already updated by the relabel) |
| matches a catalog row | differs, **no** recorded relabel | **anomaly** — barcode is meant to be immutable; refuse + surface |
| barcode matches a row, BOT uuid differs | — | **media swap under a reused label** — refuse |
| present, no catalog row | any | **rebuild-from-tape**: adopt the uuid, derive pool from the current barcode |
| absent (blank) | valid | fresh init: generate uuid, write BOT |

Keying on the uuid (not the barcode) is what lets the system *treat the barcode as
immutable while still detecting* the rare case where reality violates that —
instead of silently following a changed label or corrupting copy references. BOT
stores only the uuid (write-once); the barcode lives in the catalog and on the
physical label, so a `relabel` updates the catalog without touching the tape.

## Relabel: damaged-label replacement (the one deliberate exception)

Barcodes are immutable in normal operation, but a printed label can be physically
damaged. Rather than re-order a label, the operator applies a spare pre-printed
label and records it:

`rem tape relabel <uuid | old-barcode> <new-barcode>`
- identifies the tape by **uuid** (read from BOT) — same physical media;
- updates the catalog voltag old → new;
- marks the **old barcode `retired`** so it is never assigned to another cartridge;
- **leaves BOT untouched** (uuid is write-once — no mount-and-rewrite);
- is **audited** (who, when, old → new) — the spec's "explicit row authority"
  (`spec:1138`), used narrowly.

**Pool preservation.** Because pool is derived from the prefix, the replacement
label must carry the **same prefix** so the pool is unchanged. This drives a
barcode-scheme rule: reserve a **spare range inside each pool's prefix** (e.g.
`ACM900–ACM999` as camera.copy-a spares) so a damaged `ACM042` is replaced by an
`ACM9xx` and stays in copy-a. A replacement whose prefix maps to a *different*
pool is *also* a reassignment: refuse if the tape already holds committed copies
(safety rule), otherwise allow only with explicit confirmation + audit.

**Barcode lifecycle.** To make "mark the damaged label unused" robust, the catalog
tracks a barcode's state: `available` (pre-printed, unassigned) → `assigned` (on a
tape) → `retired` (damaged, never reuse). Init and relabel enforce it: never
assign a `retired` barcode, and never assign an `assigned` barcode to a second
tape — the explicit collision guard, with the uuid as the backstop. (A full
catalog loss forgets the `retired` list, but the uuid still prevents data
corruption and retired barcodes aren't on any live tape, so the record is
nice-to-have-durable, not load-bearing.)

## Barcode real-estate (precondition)

Standard LTO barcodes on the MSL3040 are typically **6 characters + the 2-char
media suffix** (e.g. `RMN001L9` = `RMN` + `001` + `L9`). The pool prefix must fit
in those 6 characters **alongside** any sequence and any content/copy encoding the
operator already uses. Example layout: `A`(copy) + `CM`(content=camera) +
`001`(sequence) + `L9` → `ACM001L9`, which caps a copy×content slice at ~999
cartridges and fixes the scheme for the archive's life.

**Before adopting this, confirm:**
1. The existing/planned archive barcode convention has room for a pool prefix and
   doesn't already use those positions for a conflicting purpose.
2. The prefix scheme partitions pools unambiguously and reserves growth room
   (new pools / larger sequences) within the 6-char budget, or the library is
   configured for longer barcodes if more room is needed.
3. Each pool's prefix reserves a **spare sub-range** for damaged-label replacement
   (see *Relabel*), so a replacement label stays in the same pool.

If the barcode budget can't accommodate the pool encoding, fall back to the
audited reassignment path (below) or the config-row model.

## Tradeoffs (accepted)

1. **Pool is decided at *labeling* time, not allocation time.** You lose
   just-in-time "assign any blank to whichever copy-pool is low." For a planned
   archive that provisions deliberately per pool, this is acceptable; if on-demand
   spare allocation is important, this model fights it.
2. **Failure mode shifts to labeling discipline.** A mislabeled tape lands in the
   wrong pool, fixable only via the audited `relabel` path (and, if it already
   holds committed copies, gated by the safety rule). Mitigated by validate-at-init;
   tape ops are typically disciplined about barcodes.

## Rejected alternatives

- **Per-tape config rows (status quo).** Keeps the reconcile-wipe problem and the
  init-can't-persist wart; verbose for batches.
- **On-tape membership.** Ruled out by `spec:1826` (*"Pool assignment is not
  on-tape authority… operator/orchestrator state"*) — pool is policy that can
  change, and a blank tape has no on-tape anchor. The per-copy commit snapshot
  (`object_copies.pool_id`) already preserves the historical record.
- **Audited Layer-4 assignment.** More flexible (assign/reassign any tape on
  demand, with provenance) but more machinery (assignment event type, projection
  from replay, reconcile precedence, CLI verb). Over-engineered for a write-once
  workload where tapes stay in their pool for life. Retained only as the escape
  hatch below.

## Escape hatch: audited reassignment

The audited override the spec earmarks (`spec:1138`) is realized narrowly as the
`relabel` operation above: damaged-label replacement (pool-preserving) and — with a
different-prefix replacement — genuine reassignment of an *unwritten* tape, both
explicit and audit-logged, and refused for tapes with committed copies. A fully
general "reassign any tape, including written ones, with provenance" is **not**
built; the prefix model plus this narrow relabel covers the workload.

## Implementation sketch

- Config: add `TapePoolRuleConfig { prefix, pool_id }`, ordered list; validate
  prefixes (non-ambiguous, known `pool_id`) at config load alongside existing
  pool/charset validation.
- Reconcile: replace "project configured membership rows" with "for each tape,
  match voltag against rules (longest-prefix), write the derived membership
  projection." Keep the safety rule for tapes with committed foreign-pool copies.
- Provisioning/CLI: `rem tape init` validates the voltag matches a rule; drop or
  demote `--pool` to an assertion.
- Selection (`pool-tape-selection`): unchanged — it reads the derived membership.
- Identity: generate `tape_uuid` at first init, write it to BOT (write-once), use
  it as the catalog PK; re-init matches by uuid per the identity table above.
- `rem tape relabel`: update the catalog voltag for a uuid, retire the old barcode,
  enforce pool-preservation (same prefix) + the committed-copy safety rule, and
  audit-log the change.
- Barcode lifecycle: track `available`/`assigned`/`retired`; enforce at init +
  relabel.
- Follow the normal gates (`fmt` / `clippy -D warnings` / `check` / `test`); the
  new Rust surface is a plain serde config struct + a reconcile function change,
  so no trait/lifetime design risk.

## Storage (v0.1 storage refinement)

Membership is the single config-driven projection of `[[tape_pool_rules]]` and is
stored as the `tapes.pool_id` column, not a separate table. The static
`[[tape_pool_memberships]]` input path has been removed: config
`[[tape_pool_rules]]` is the single source of truth for membership.
`bringup/rem-init.sh` config generation is normal provisioning, not a workaround.
See `docs/pool-membership-storage-design-v0.1.md`.

## Open questions

1. Confirm the barcode convention (precondition above) — this gates the whole
   decision.
2. Multi-field prefixes: if pools encode copy **and** content, is the prefix a
   single string or structured fields (`copy` + `content`) composed into a
   `pool_id`? Single-string longest-prefix is simplest; revisit if the scheme
   needs structure.
3. Exact-match vs prefix for some pools (e.g. a quarantine pool keyed to specific
   voltags) — allow an exact-voltag rule as a longest-match special case.
4. Barcode-inventory scope for v1: full `available`/`assigned`/`retired` lifecycle,
   or just enforce `assigned`/`retired` at init/relabel and leave `available` stock
   to operator management?
5. `relabel` in the first implementation slice vs a fast-follow — damaged-label
   replacement is the realistic near-term trigger.
