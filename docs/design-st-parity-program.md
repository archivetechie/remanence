# st-parity program — inheriting thirty years of tape lore without giving up control

**Status: BRIEF v0.1 (2026-07-08) — for panel review, then codex audit dispatch.
Sequenced BEFORE/WITH TIO-5 (its deferred-error chapter is TIO-5's reference).**

## The risk

rem drives tape via sg (raw SCSI passthrough) for a load-bearing reason: the
commit protocol, audit log, quarantine fences, and refuse-clobber ladder all
require raw sense visibility and precise positioning that the kernel st driver
deliberately hides. The cost: st embodies ~30 years of accumulated handling for
tape reality (quirky drives, deferred errors, unit attentions, EOM semantics,
timeout lore) that rem reimplements from spec + its own testing. Spec-derived
code handles the spec; battle-tested code handles the world. For a generational
archive, the unknown-unknowns gap must be actively managed, not assumed away.

**Calibration from the 2026-07-07b window**: ~a dozen rough edges surfaced (UA
one-shots un-retried, sticky fences, daemon world-model drift after restart with
mounted media); every one failed closed; zero integrity damage. The exposure is
operational friction, not durability — because rem's structural posture is
detect-and-heal (content digests, read-back verify, scrub, multi-copy,
self-heal), which st users don't have at all. This program narrows the friction
and hardens the posture; it does not rest on handling perfection.

## Why not st for the data path

st absorbs sense data, retries silently, and reports buffered-write errors
detached from their cause — features for backup software, disqualifying for a
commit protocol that must prove what happened. Also: our scope is 1–2 drive
families / 1 HBA family / pinned RHEL — st's breadth is mostly not our risk;
its depth per behavior class is, and that transfers (below).

## The program (five legs)

### 1. Behavioral audit → conformance matrix (codex, prompt below)
Extract from `drivers/scsi/st.c` (+ st.h, SCSI mid-layer interactions) a catalog
of behavior classes; map each to rem's equivalent in remanence-scsi /
remanence-library: **matched / deliberately-different (rationale) / GAP
(risk-ranked P0–P3)**. Behavior classes, minimum set:
- Deferred-error machinery (buffered writes/filemarks; CHECK CONDITION with
  deferred sense; association of late errors to earlier operations) — TIO-5's
  reference semantics
- Unit-attention taxonomy + retry policy (POR, media-changed, mode-changed,
  reservation preempt; which auto-retry, which surface)
- Early-warning / EOM contract (EW-on-write handling, ENOSPC semantics, writes
  between EW and physical EOM, LEOM vs PEOM)
- Timeout ladder per command class (write/read vs space/locate vs rewind vs
  erase vs load/unload) — steal the constants
- Reset / reservation-loss recovery (position invalidation, re-validation)
- Position cache invariants (when trusted, when re-read); fixed-vs-variable
  block mode transitions; READ BLOCK LIMITS negotiation
- Close-time semantics (trailing filemarks, rewind-on-close variants)
- Partition handling; density/compression mode-page discipline
- Any drive-quirk tables/flags and what triggered them historically
Provenance discipline: **semantics only, no code transcription** (GPL).
Deliverable: `docs/report-st-conformance-matrix.md`.

### 2. Gaps → executable oracle tests
Every GAP row becomes a ModelTransport sense-injection test, a chaos-phase
fault, a harness scenario, or (pure kernels) a Lean obligation. st's decades
compress into an oracle suite that runs on every commit — stronger than
inheriting code, which can regress silently. Owner: prompt cut per P0/P1 gaps
after the matrix lands.

### 3. Lore subscription (gardener leg)
Watcher on kernel tape-subsystem commits (st.c, scsi SSC paths) + HPE/IBM LTO
firmware release notes → monthly digest into STANDING/backlog: each upstream
fix is evidence of a real-world failure someone else paid for. Converts
community battle-testing into a standing advisory feed.

### 4. Hardware-qualification policy
The fieldtest kit is the qualification suite: any new drive model, firmware
level, HBA, kernel/RHEL bump runs the kit (init, happy path, crash re-proof,
bench, fault subset) before production trusts it. Add to STANDING conventions;
drift = re-qualify.

### 5. Fence UX hardening (already on backlog, folded here)
The window's friction list (auto-resolve for settled fences, UA auto-retry
per class per the audit's policy table, bringup drive-occupancy reconciliation,
magazine-out as transient `motion-paused` state) — implemented per the audit's
policy tables so the fixes inherit st's semantics rather than ad-hoc choices.

## Sequencing

matrix audit (codex, ~days) → fold + risk-rank (panel light) → P0 oracle tests
+ TIO-5 design (consumes deferred-error chapter) → lore watcher + qualification
policy (gardener/process, parallel).

## Acceptance

- Conformance matrix covers the behavior-class list with zero unclassified rows
- Every P0/P1 gap has an oracle test or an explicit accepted-risk row signed in
  STANDING
- TIO-5 design cites the deferred-error chapter for its filemark semantics
- Lore watcher produces its first digest; qualification policy in AGENTS/process
