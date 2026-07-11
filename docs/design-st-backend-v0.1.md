# Design — optional `st`-native tape backend (`linux-st`) — decision record + kickoff v0.1

**Status:** **ACCEPTED-CONDITIONAL** (2026-07-11) — build approved, **sequenced AFTER
TIO-6 R2 + automated verification land**. This is a decision record + kickoff, not the
implementation design; the detailed design is cut when the build starts. Owner (the owner)
approved the two-model panel conclusion 2026-07-11.

**Panel:** two independent reviewers, blind — **Fable** and **codex gpt-5.6-sol** — both
returned **PURSUE-CONDITIONALLY, after TIO-6, as a fenced MVP** (brief +
transcripts: this session; evaluations summarized below). Two different model families
converging is the confidence signal.

---

## 1. Decision, and the corrected justification

Add an **optional, per-copy-selectable tape backend that reads/writes via the Linux `st`
driver** (`/dev/nstN` + `MTIOCTOP`), alongside the primary bespoke SG-based direct-SCSI
path. Build it — but **not on corruption-safety grounds, and not now.**

**The reframe both models forced (load-bearing):** the original motive was "our
days-old SCSI driver might silently corrupt an important archive." That motive is
**already covered** by two facts established this session:
- **RAO plaintext objects are POSIX pax tar** (`reference-tape-layout.md`): extractable
  with stock `tar`, per-file SHA-256 in the pax headers, CBOR manifest — "the
  30-year-readability property is not a promise, it is the format." So **recovery
  independence is a property of the FORMAT, present on every plaintext copy**, not
  something `st` uniquely supplies.
- **Independent readback-verify** (stock `tar` extract → `sha256sum` → compare to
  card-offload source hashes) is a stronger integrity proof than *either* driver's
  successful return, and it catches an SG write-corruption bug directly.

gpt-5.6-sol, bluntly: *"under mandatory verification, an `st` backend contributes almost
no additional probability of accepting corrupted media."* So **`st` is NOT justified as a
data-integrity hedge.** Its real, verification-independent value is:

1. **Availability / compatibility.** Verification can't make the SG path *work* on an
   unfamiliar drive/HBA/firmware, and can't keep production running through an SG
   regression. `st` can. rem's readiness/timeout constants are MSL3040-specific; `st`
   shifts low-level compatibility into a mature, Linux-maintained kernel path.
2. **The SG-hardening differential oracle** (both reviewers flagged this *independently*
   — the strongest agreement). Writing the same RAO object via `st` **and** SG and
   cross-reading (`st`-write→SG-read, and vice versa) is executable conformance testing
   for the **primary SG path**. Much of the build's value lands on making our main path
   more trustworthy — it is partly test infrastructure, not a second product.
3. **Adoption / trust flywheel.** For an AGPL project courting external operators,
   "start on the kernel driver you already trust; the SG path is an upgrade, not a
   prerequisite" lowers the barrier → more real-world users → the whole stack (format,
   catalog, orchestrator) gets battle-tested faster.

**Market and justify it as a compatibility / availability / trust backend — explicitly
NOT as the source of preservation-independence, and NOT as permission to skip
verification.**

## 2. What `st` is — and is NOT — for the durability policy

`st` gives a separately-evolved kernel interpretation of tape errors, buffering, reset
state, filemarks, and early warning — meaningful diversity from fresh rem SG code. But
it is **NOT a second implementation family**: it shares rem's formatter, parity layer,
catalog, orchestrator, and source data, **and** the Linux SCSI midlayer, HBA, drive
firmware, and media. Therefore:

- Record `rem-sg` vs `linux-st` as an **additional provenance axis**, NOT
  implementation-family credit.
- **`st` MUST NOT count toward the ≥2-implementation-family durability floor, and MUST
  NOT authorize retiring `d2tape`.** `d2tape` (separate format *and* code) remains THE
  family-diversity leg. (Panel note: Fable framed `st` as d2tape's *successor*;
  gpt-5.6-sol rejected family credit; both agree **keep d2tape** — do not rush its
  retirement. Resolved in favor of the stricter accounting.)

## 3. Binding invariants

1. **Never weaken SG.** Preserve SG's `DevicePositionProof` / fencing contracts
   unchanged. `st` position is a **weaker kernel-reported/computed HINT**; never cast it
   into a `DevicePositionProof`. Higher layers explicitly decide which operations are
   legal under weaker evidence. **If adding `st` requires weakening SG invariants
   globally, STOP** — that reverses the safety value of the optional backend.
2. **Wrap, don't copy** (project additive-bias rule, with force): route `st` outcomes
   through the *existing* catalog/audit/fence funnel; do not fork a parallel safety path.
3. **Verification stays authoritative.** A completed `st` write is `written_unverified`;
   only a successful **full source-hash readback** promotes it to `verified`. An
   unverified copy MUST NOT satisfy minimum-copy policy, authorize source deletion, or
   justify retiring another copy. Any emergency "accept unverified" exception is durable,
   visible, and **alarmed** (Drishti). If verification is being skipped routinely, the
   defect is capacity/process design — `st` is not the repair.
4. **No mid-file/mid-copy failover** between SG and `st`. Backend is chosen per copy up
   front; exclusive drive ownership (never concurrent SG + `st` on the same drive).

## 4. Fenced MVP scope (both reviewers converged near-identically)

Intentionally austere; a fenced first cut, not a feature-equivalent second driver:
- Linux-only, **compile-time optional**; explicit per-copy pool selection; **no
  automatic fallback**.
- `/dev/nstN` matched to the configured **drive serial**, not an unstable device number.
- Fixed-block, partition-0 LTO only; large aligned writes; **immediate filemarks
  disabled**.
- **Same RAO format** — plaintext RAO first (pax-tar), RAO1 encrypted later once its
  independent decrypt-and-hash verification is automated. **No new "st format."**
- **Blocking `MTWEOF` at every commit boundary** as the sync/error-collection barrier
  (Linux docs treat a non-immediate filemark as a flush+error-catch point). Explicit
  finalization that reports flush/close errors — **never rely on Rust `Drop`.**
- **Compression explicitly disabled and verified** before any parity-protected write.
- Fresh / fully-rewritten tapes and sequential sessions initially.
- On any reset / ambiguous EOM / lost position / process crash / close error:
  **fence-quarantine the copy and rewrite/reconcile — no clever resume in MVP.**
- Reads: enough sequential read for verification + recovery testing; **restores of
  `st`-written tapes route through the existing SG read path** (same records, same
  filemarks) — the backend needs no read path of its own for MVP.
- Provenance record per copy: backend, kernel version, `st` options, drive/HBA identity,
  verification reader, hashes, verification timestamp.
- NOT in MVP: random locate, parity repair, crash-resume append, nuanced early-warning
  continuation.

## 5. The differential oracle (ship from day one)

Periodic qualification MUST **cross the transport paths**: `st`-write → SG-read, and
SG-write → `st`-read, then byte/hash-compare against source. (`st`-write followed only by
`st`-read retains common-mode driver risk.) For stronger independence, verification
reopens/reloads the tape and uses an independent RAO/tar decoder (stock `tar` for
plaintext). This is half the build's value — it hardens the SG primary path.

## 6. Acceptance gates (not "supported" until all pass)

1. Byte/layout equivalence against SG-produced media.
2. **Full** source-hash round-trips (not sampled).
3. Implicit close-filemark + duplicate-filemark-avoidance tests.
4. Buffered/deferred error surfacing at `MTWEOF`.
5. ENOSPC / EW / EOM + partial-write handling.
6. Reset, disconnect, process-kill, lost-position fencing.
7. Compression-disable + block-size readback.
8. Reopen / unload / reload verification.
9. Cross-read: SG reads `st` media and `st` reads SG media.
10. Physical filemark durability.
11. **At least the current LTO-9 rig PLUS one materially different drive/HBA family**
    before making any broad-hardware / adoption claim.

**Go/no-go:** if a prototype cannot meet these **without pervasive backend branching**,
abandon it or retain it only as a **break-glass import/export tool** — not a supported
backend.

## 7. Sequencing

- **Design the seam now** (capability-aware `RawTapeSink`/`RawTapeSource` contract that
  admits a weaker-evidence backend without contaminating SG proofs); the existing
  `RawTapeSink`/`RawTapeSource` + the `d2tape` backend precedent show the pool/backend
  seam already accommodates a foreign writer.
- **Implement AFTER TIO-6 R2 + automated verification land.** Rationale (both reviewers):
  the same engineering hours buy more safety by finishing TIO-6, making verification
  un-skippable, and qualifying SG on a second drive family. No urgency — `d2tape` covers
  the driver-provenance axis today.

## 8. Open questions for the build-time design

- Exact capability contract: how higher layers gate operations on `st`'s weaker position
  evidence (which ops are legal, which refuse).
- `st` option matrix (buffering, `MTSETDRVBUFFER`, `MTSETBLK`, EW behavior) and how it's
  pinned + recorded per write.
- Whether `d2tape`'s eventual retirement is reconsidered given `st` (panel: **no** — keep
  `d2tape` for format-family diversity; revisit only if a *second format family* is
  added elsewhere).
- Encrypted-RAO independent verification tool (standard HKDF-SHA-256 + ChaCha20-Poly1305
  decrypt outside remanence, then hash) — prerequisite before RAO1 via `st`.

## Provenance

Facts: `reference-tape-layout.md` (RAO = pax tar), `report-st-harvest-2026-07-10.md`
(st source-verified; async-error-detachment disqualified it as PRIMARY; 246–293 MB/s via
dd), the ≥3-copy/≥2-family durability policy, `d2tape` (tar→`/dev/nst0`). Panel: Fable +
codex gpt-5.6-sol, 2026-07-11, both PURSUE-CONDITIONALLY; the sole disagreement
(family-credit vs provenance-axis) resolved in §2 toward the stricter accounting.
