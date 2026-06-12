# Why tapes need a lifecycle — the recycle problem, explained

**Companion to:** `docs/tape-identity-lifecycle-design-v0.1.md` (the
normative design). This document is the *why*: what actually goes wrong
today, walked through step by step, and why the fix has the shape it
has. Nothing here is normative.

---

## 1. A tape has two names, and they mean different things

Every cartridge in the library carries two identifiers:

- **The barcode (voltag)** — `RMJ101L9` — a sticker on the plastic
  shell. The robot's scanner reads it; operators read it; it names the
  *physical object*. It can be peeled off, replaced, or (through human
  error) duplicated. It survives erasure of the tape.
- **The BOT uuid** — `5e8f8d3b-…` — a UUID inside the bootstrap block
  that `rem tape init` writes at the very beginning of the tape
  (Beginning Of Tape). It names a *logical tape* — not the plastic, but
  one **life** of that plastic: this geometry, this pool, these
  objects. It is unreadable without mounting the tape in a drive.

A useful analogy: the barcode is the **license plate**, the BOT uuid is
the **VIN of one car**. Plates move between cars; a VIN never does.
When you wipe a tape and re-initialize it, you haven't reused the old
logical tape — you've created a *new* one living in the same shell.
The old life is over; its data is gone; anything that still refers to
it refers to something that no longer exists.

The catalog (`rem-state.sqlite`) records the marriage:

```text
tapes:  uuid 5e8f8d3b…  voltag=RMJ101L9  state=ready  pool=amber-aof
```

And the write path uses it as a safety check: before any byte is
written, the daemon mounts the tape, reads the BOT bootstrap, and
compares the uuid it *found* against the uuid the catalog *expects*
for the tape it selected. This is the system asking: **"is the tape in
this drive really the logical tape I think I'm writing to?"**

That check is not bureaucratic. On a physical library it is what stands
between you and the worst archival accident there is — writing over the
wrong tape (§4).

## 2. What goes wrong today — three walk-throughs

### Scenario A — the VTL rebuild (your dev box, every flaky rerun)

Day 1, everything green:

```text
Catalog:  RMJ101L9 → uuid X   state=ready    (after rem tape init)
Medium:   BOT bootstrap uuid X
          → identity check: expected X, found X ✓ writes succeed
```

Then you rebuild the QuadStor VTL. The virtual cartridges come back
**blank** — or get re-initialized in some scenario, gaining a fresh
bootstrap with uuid **Y**. The catalog, of course, was not consulted:

```text
Catalog:  RMJ101L9 → uuid X   state=ready    (stale)
Medium:   blank, or BOT uuid Y
```

Now run the scenario again:

1. The pool selector consults the **catalog** and happily picks
   RMJ101L9 (catalog says: ready, has capacity).
2. The daemon mounts it, reads BOT, and the check fires exactly as
   designed:
   `FAILED_PRECONDITION: tape identity mismatch: expected 5e8f8d3b…, found 3b158ca7…`
   (or `absent bootstrap at BOT` for the blank case).
3. You think: fine, re-init it. But `rem tape init RMJ101L9` *also*
   consults the catalog, sees the barcode is already married to uuid X
   while the medium says Y — and classifies that as an **anomaly**:
   `tape init RMJ101L9: refused-no-write — anomaly: barcode assigned to 5e8f8d3b…; BOT uuid=3b158ca7…`.
   Crucially, the anomaly arm has **no override flag at all** — not
   `--force`, not `--clobber-data`. There is deliberately no way
   through it (see §4 for why).
4. Every tape in the pool is in the same condition, so eventually:
   `RESOURCE_EXHAUSTED: pool amber-aof has no writable tapes (N rejection(s))`.
5. The only door left is the sledgehammer: `make reset` →
   `rem-debug catalog reset --i-understand-this-erases-the-catalog`.
   It "works" — by erasing **everything**: not just the stale binding
   but the entire SQLite catalog, the append-only **audit history**
   (the tamper-evident record of every operation ever performed), and
   the 3c journals. You amputated an arm to fix a hangnail.

### Scenario B — recycle scripts, back to back

`recycle-scenario-*-tapes.sh` re-initializes tapes between scenario
runs. First run after a recycle: the script calls `rem tape init`,
which hits the same anomaly as Scenario A step 3 — because the catalog
still remembers the *previous* run's uuid for that barcode. The script
cannot self-correct; the rerun is flaky; the harness now does a full
`reset` + `up` between runs as a workaround. That works only because
the dev box's catalog is disposable. The same sequence on a production
catalog would mean destroying the audit history every time a tape is
recycled — obviously unacceptable.

### Scenario C — the future one: self-heal (system Scenario Q)

The multi-copy plan: every object lives on ≥2 tapes; lose one tape,
re-replicate from the survivor. Walk it through with today's code:

1. Tape with copies of objects A, B, C dies (or is wiped for reuse).
2. You want to tell the system: *"that tape's contents are gone; which
   objects are now down to one copy? Re-replicate them, and let me
   reuse the cartridge."*
3. Today there is no way to say any of that. The catalog still lists
   the copies as `committed` on uuid X. Re-initializing the cartridge
   hits the anomaly. Nothing can enumerate "objects whose copies are
   gone" because nothing can *record* "these copies are gone."

Self-heal is blocked on exactly the missing primitive: a way to end a
tape-life and account for its consequences.

## 3. Why the obvious fix is wrong

"Just make the identity check auto-update the catalog when it finds a
different uuid" — that *would* make every scenario above green. It
would also destroy data in the one scenario that matters most:

### Scenario D — the mislabeled cartridge (production, MSL3040)

A human re-shelves cartridges after maintenance and two barcode labels
get swapped (it happens; it's the *reason* enterprise software
distrusts barcodes). Or a second system's tape — dwara2 shares the
chassis — ends up in a Remanence-visible slot wearing a reused label.

Now the catalog says `RMJ101L9 → X` and the medium in the slot carries
uuid `W` — **a live tape full of someone's data**. The write path
selects RMJ101L9, mounts it, and finds W:

- **Today:** identity mismatch → refuse. The data on W survives. The
  operator investigates. This is the system working.
- **With auto-rebind:** the catalog "reconciles" to W, the write
  proceeds, and tape writes truncate everything downstream of the
  write point. Someone's archive is now destroyed *by the safety
  system* — silently, with a green exit code.

The machine cannot distinguish Scenario A (medium legitimately
replaced) from Scenario D (label lies; medium is precious). They look
*identical* from the drive: barcode says one thing, BOT says another.
The only thing that distinguishes them is **operator intent** — which
is why the fix is a ceremony a human (or a script a human wrote)
performs explicitly, and why the default must stay refusal.

## 4. The fix: a death certificate for a tape life

The design adds one operation. In words: *"I, the operator, declare
that the logical tape X is dead. Its contents are gone on purpose. Stop
expecting them. Free its barcode for a new life."*

```sh
rem tape retire RMJ101L9 --reason recycled \
    --i-understand-copies-become-unreadable
```

What it does — five effects, one transaction plus one audit record:

1. The tape row flips to **`state='retired'`** — a terminal state. The
   pool selector will never pick it again (anything not `ready` is
   unwritable already).
2. The **barcode is released** (`voltag` cleared). The catalog enforces
   barcode-uniqueness, so the old marriage must end before the same
   sticker can be bound to a new uuid.
3. Every copy the catalog had on that tape flips from `committed` to
   **`missing`** — the honest answer to "can I restore object A from
   tape X?" after X was wiped. Objects with copies elsewhere remain
   restorable; objects with *no* committed copy left become visible as
   **degraded** — which is precisely the work-list self-heal needs
   (the design adds the query for it).
4. An **audit event** (`TapeRetired`) is appended to the hash-chained
   log: who, when, which barcode, why, and how many copies were
   declared missing. A destructive declaration belongs in the
   permanent record (this also chips at a review finding — today
   `tape init` leaves no audit trace at all).
5. The retired row and its history **stay in the catalog forever**.
   Retire is not delete. Nothing is erased — the point is to *record*
   reality, not to rewrite it.

And then `rem tape init RMJ101L9` just works:

- the barcode is free, so the blank-medium case is a plain fresh init;
- if the medium still carries the *retired* uuid's old bootstrap and
  data, init recognizes "this medium's previous life was retired" and
  proceeds as a fresh init **without demanding the `CLOBBER` rite** —
  you already performed the ceremony; asking twice for the same intent
  is friction without safety;
- but if the barcode is bound to some *other live* uuid, or the medium
  carries a bootstrap the catalog has never met — still **refusal**,
  exactly as today. Scenario D stays defended. Retire whitelists one
  specific dead identity, never a category.

The old vs new flow, side by side:

```text
TODAY  (per recycled tape)              WITH RETIRE
─────────────────────────              ───────────────────────────────
rem tape init  → anomaly ✗             rem tape retire RMJ101L9 \
make reset     → catalog,                  --reason recycled --i-… ✓
                 audit log,            rem tape init RMJ101L9      ✓
                 journals: erased      (catalog, audit, journals:
make up                                 intact; audit GAINED a record)
rerun scenario
```

## 5. The subtle part: why this touches the rebuild path

This is the piece that makes the design longer than "one UPDATE
statement," and it's worth understanding because it's the same trap the
code review caught elsewhere (finding H2).

Remanence's persistence principle is: **SQLite is disposable**. The
durable truth lives in the append-only audit log and the per-tape 3c
journals; the catalog can be wiped and rebuilt from them at any time
(`rem rebuild-catalog-from-journals`, and the daemon does a replay at
startup). Now follow a retired tape through that rebuild:

1. Tape X was written once: its journal says "objects A, B committed
   to X." That journal is *history* — it stays on disk forever, and it
   is correct: those objects *were* committed to X.
2. You retire X (medium wiped). Catalog row: `retired`, copies
   `missing`.
3. Months later, a rebuild runs. It clears the tapes table and replays
   the journals… which faithfully resurrect X as an *ingested* tape
   with *committed* copies. **The retire just silently un-happened.**
   The pool won't write to it (state `ingested` isn't writable), but
   restore planning once again believes copies exist on a wiped
   medium — and the moment anything re-inits or reconciles that
   cartridge, you're back in Scenario A.

I call this the **resurrection trap**: any identity-lifecycle fact that
lives *only* in SQLite is erased by the very rebuild machinery the
system is proudest of. (Finding H2 was the same trap for `voltag`/
`ready`/`sealed`; codex's in-flight fix added a "preserved rows"
mechanism that carries operator-set columns across rebuilds.)

So the design makes `retired` survive rebuild the same way
`sealed` now does — the preserved-rows snapshot keeps it, the journal
replay is told not to overwrite it — and then *re-derives* the copy
statuses from it: after replay, every copy on a retired tape is marked
`missing` again. That last bit is a deliberately boring rule worth
appreciating: **copy status is a consequence of tape state, never an
independent fact** — so there's exactly one thing to preserve and no
way for the two to drift apart.

(Why not just delete the journal at retire time? Because the journal is
the authoritative history that A and B *were* written there — the audit
trail of the system's past. Retire records that the present changed; it
must not falsify the past. Append-only history + a state that overrides
its present-tense implications is the design that keeps both true.)

## 6. What retire deliberately does NOT do

- **It does not touch the tape.** No mount, no robot, no SCSI. The
  medium may be in a drive, a slot, or a landfill. It's a catalog +
  audit operation about *expectations*.
- **It does not erase anything** — not the row, not the journal, not
  the copies' rows. Everything becomes history with an honest status.
- **It is not undoable.** There is intentionally no `un-retire`: if you
  retire the wrong tape, the medium's data is still physically intact —
  but the way back is to re-ingest it under a *new* identity (mount,
  reconcile/scan, recatalog), not to flip a bit back. An undo flag
  would turn the death certificate into a toggle, and every safety
  argument in §3 assumes it isn't one.
- **It does not bypass any guard for unknown media.** A tape the
  catalog never knew, or a barcode bound to a different live tape,
  refuses exactly as before.

## 7. What you'll see day to day

Before → after, on the dev box:

| Situation | Today | After |
| --- | --- | --- |
| Scenario rerun after VTL rebuild | `tape identity mismatch` → `make reset` (audit history destroyed) | `rem tape retire` each affected barcode → `rem tape init` → rerun. Catalog and audit intact. |
| `recycle-*-tapes.sh` | flaky without full reset | scripts call retire+init; idempotent (re-running retire on a retired tape is a no-op success) |
| "which objects lost all copies?" | unanswerable | the degraded-objects query (self-heal's input; surfacing in CLI comes later) |
| Mislabeled / foreign cartridge | refused | **still refused** — unchanged on purpose |

And one piece of reassurance about something the concern doc flagged:
the "daemon crashes" seen during re-validation were clean `systemd`
stops performed by the tooling itself (scripts stop the daemon; some
make targets don't restart it). The design includes making those
scripts symmetrical — but there was no crash, and retire doesn't need
the daemon at all (it takes the same local state lock the CLI already
uses).

## 8. The one-sentence version

The catalog could record that a tape *exists* but not that it *died*;
the only way to register a death was to burn down the registry — so we
are giving tape identities a death certificate (`retire`) that is
audited, survives catalog rebuilds, frees the barcode for the next
life, and tells restore and self-heal the honest truth about which
copies are gone — while leaving every refusal that protects real data
exactly as paranoid as it is today.
