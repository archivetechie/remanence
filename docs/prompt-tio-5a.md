# Prompt TIO-5a — pipelined submission, write side (staging ring + hot submitter + in-place accounting)

**Status:** pending (cut 2026-07-10 from FROZEN design v0.4).
**Normative source — read first and treat as binding:**
`docs/design-tape-io-pipelined-submission-v0.1.md` (v0.5). Where this prompt
and the design disagree, the design wins. Frozen constraints it inherits:
`docs/design-tape-io-throughput-v0.1.md` (TIO-1..4 error machinery + commit
ordering — MUST NOT change), `docs/layer5-multi-object-append-design-v0.1.md`
(durable-boundary ordering), `docs/lto9-media-readiness-design-v0.1.md`
(fence/dirty-scope rules).

## Scope (design §10 item 1 — write side only; read side is TIO-5b)

1. **Staging ring** (design §3.1): fixed pool of `staging_ring_buffers`
   (default 4, validate 2..=16, checked allocation, log effective per-drive
   ring bytes at open) batch-sized buffers. Stager (evolves the TIO-3
   producer) reads spool **directly into** ring buffers (no per-batch Vec, no
   intermediate copy), pre-builds fixed-mode WRITE(6) CDBs; the **trailing
   partial batch rebuilds its CDB** for its true record count. Buffer-return
   path capacity ≥ ring size (return never blocks). Page-align buffers
   (rationale: future O_DIRECT spool reads — sg indirect I/O needs none).
2. **Submitter hot loop** (design §3.2) on the existing drive-actor thread:
   pop → `set_timeout_for(TapeIo)` (KEPT per command — the v0.1 hoist was
   rejected; it self-heals the class after inline RPs) → SG_IO → status
   — **and raise the constants (st-harvest F3): `TimeoutClass::TapeIo`
   60 s → 900 s, `TimeoutClass::WriteFilemarks` 120 s → 900 s** (st's
   normal timeout, sized for drive-side recovery; zero application retries
   unchanged; classes stay distinct; all other TimeoutClass values
   unchanged). Do this BEFORE cutting equivalence fixtures
   check → record GOOD completion (in-place) → advance cursor → tripwire
   check → return buffer → repeat. The good path is a **new audit-free
   variant**; extract the TIO-1/2 decode helpers (`write_eom_signal`,
   `fixed_records_transferred_from_sense`, `records_delta_between`,
   deferred-sense classification) and reuse them **unmodified**.
   **Cursor arithmetic split from tripwire execution**: GOOD data command
   recorded before any tripwire RP is issued; the tripwire is its own
   recorded op.
3. **Classified non-GOOD split** (design §3.2 — three classes, st-harvest
   F1/F2 folded):
   - **successful-with-signal**: full-record EW ⇒ success with
     `early_warning` flags, NO fence, NO stop, propagates to pool-write
     policy as today; **current sense key RECOVERED ERROR (0x1) with no
     terminal flags ⇒ success, audited-as-recovered**; RECOVERED ERROR +
     EOM ⇒ the EW class via the exact pre-position/post-RP-delta
     arbitration. Deferred 0x71/0x73 never enters this class;
   - **state-invalidating current sense**: UNIT ATTENTION `06/29/00..04`
     (power-on/reset) ⇒ stop pipeline, no further CDBs, invalidate
     `expected_position` AND cached mode validation, fence any active
     uncommitted session before audit/propagation; recovery only via
     readiness admission + device position proof + MODE SENSE
     re-verification of the tape-sourced block size;
   - **safety-relevant** (partial batch, hard EOM, undecodable residual,
     deferred sense 0x71/0x73, transport-unknown, tripwire mismatch) ⇒
     **classify → fence → audit → propagate**; fence/position-invalidation
     never wait on audit emission.
4. **In-place accounting** (design §4): preallocated counters + fixed
   histogram buckets (`gap_us`, `ioctl_us`) bumped by the submitter; one
   coalesced TapeWrite span per staging window emitted synchronously;
   errors/EW/EOM/recovered-error completions/reset-UA state
   invalidations/tripwire RPs/filemarks/fences/session open-close audited
   individually inline exactly as today (sense bytes preserved); the
   two-tier equivalence's preserved-1:1 list includes the recovered and
   reset-UA classes;
   **per-window intent marker** (planned CDB range) before entering the hot
   loop. Completion records own their data by value (no ring-buffer
   borrows). NO companion thread, NO accounting queue, NO drain barriers.
5. **Terminal poison protocol** (design §3.1): on any error/poison, close
   the submit channel BEFORE any join; drain-and-discard queued buffers
   without issuing CDBs; parked stager always released; ring accounting
   balances (no leak/double-return).
6. **Config** (design §7): `tape_io.pipelined_submission` (bool,
   **default false**), `tape_io.staging_ring_buffers` (default 4). Backout
   ladder `legacy_single_block` > `pipelined_submission`. Effective mode
   exposed in live status/diag.
7. **Model-transport timeout-class recording** (new test infra): the model
   transport must record the timeout class per command so timeout
   regressions are visible to tests (today `set_timeout_for` is a no-op
   there).
8. **Diag** (design §8): p50/p95/max `gap_us`, cadence, effective feed
   rate, alongside existing `effective_batch_blocks` / `position_calls`;
   readable from shared diag state, never via a `DriveCommand`.

## Tests (design §9, write-side rows — all of these)

- single-command-in-flight assertion under pipelining;
- two-tier equivalence: OFF vs shipped TIO-3/4 exact (CDB + timeout-class +
  audit-event-sequence + poison); ON vs OFF = byte-identical CDB + identical
  timeout-class streams + normalized audit trace (§4's individually-audited
  classes 1:1; only good data-WRITE Started/Finished fold into intent
  marker + coalesced span, counts/bytes reconciling);
- timeout-class regression around tripwire and error-path RPs (success,
  mismatch, RP-failure exits — next WRITE always TapeIo/900 s);
- recovered-error matrix (design §9): `0x70/key=1` no flags → success +
  audited-as-recovered, no fence/stop; `key=1+EOM` → EW class via RP
  delta; `key=1+ILI/FM` → respective paths; deferred `0x71/0x73` key=1 →
  completion-unknown always;
- reset-UA state invalidation (design §9): `06/29/xx` on WRITE/READ/
  FILEMARK and after a GOOD batch → stop, cursor + mode validation
  invalidated, session fenced pre-audit, not classified deferred; recovery
  requires position proof + MODE re-verification;
- EW × tripwire interleave with low `position_check_bytes`
  (`position_before` pin proven fresh);
- trailing partial batch CDB rebuild (N not a multiple of B);
- GOOD-record-before-tripwire, split: failure injection (record survives,
  fence sees both records) vs kill injection (no survival claim; recovery
  per design §6 crash rows 2–5, each kill point mapped to its row);
- non-GOOD mid-stream split: full-record EW (no fence, no stop, policy
  propagation) vs safety-relevant (stop, fence-before-audit asserted with a
  failing/slow audit sink);
- terminal poison protocol (parked stager released on every error path;
  close-before-join; ring balance);
- ring config bounds (reject 0/1/>16), checked allocation, non-blocking
  return;
- crash-table rows (design §6) via chaos kill injection, write-side;
- `pipelined_submission=false` reproduces TIO-3/4 exactly (four-way
  equivalence, not CDB-only); `legacy_single_block=true` still reproduces
  the original serial stream; effective mode visible in status.

## Definition of done (AGENTS.md applies)

- `cargo test` green across the workspace; `cargo clippy` clean;
  daemon socket tests run (sandbox limits are REPORTED in your summary,
  never encoded as `#[ignore]` — hunks doing so will be reverted);
- on-tape format unchanged; layer-5 commit ordering untouched
  (data → blocking filemark → DevicePositionProof → journal fsync → SQLite);
- no public-API breakage for pool_write callers beyond the config keys;
- summary lists: files touched, invariants covered by which tests, any
  deviation from the design (should be none — raise instead of deviating).

**Verification member:** hermetic suite above + the harness acceptance hook
(`~/system` `scenario-append` gains `covers: rem.tape.pipelined_io` once
5a+5b land — system-side one-liner, tracked there). Physical §8 acceptance
runs at the next MSL3040 window, not in this prompt.
