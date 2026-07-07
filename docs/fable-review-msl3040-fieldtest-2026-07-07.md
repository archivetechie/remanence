# Fable review — MSL3040 fieldtest and media-readiness follow-up

**Status:** current (review verdict + two work tracks).
**Date:** 2026-07-07.
**Reviewer:** Claude Fable 5, responding to
`docs/fable-brief-msl3040-fieldtest-2026-07-07.md`.
**Inputs:** the brief, `docs/lto9-media-readiness-design-v0.1.md`,
`docs/layer5-multi-object-append-design-v0.1.md`, a full source map of the
readiness/fence/session-lifecycle implementation, and primary vendor
documentation on LTO-9 media optimization.

---

## 1. Verdict

The testing framework is fundamentally sound. Probe-first admission,
fence-only-when-not-ready, durable readiness operations, fail-closed
destructive escalation, and evidence-first field scripts are the right
architecture, and the physical results so far (4-tape init, happy path,
6-object dense append with full SHA-256 read-back fidelity) validate both the
multi-object append design and the readiness design on real hardware.

The headline finding: **the repeated media-readiness fences during
`13-append-loop.sh` are not the readiness machinery being over-conservative —
they are the field tooling doing a full physical unload/reload cycle per
object.** The fence is the messenger, not the cause. The production-latency
concern is therefore mostly misdirected at the fence; the real levers are
session reuse and dismount policy (Track A) plus a poll-cadence fix (Track B).

## 2. The load-bearing mechanism

Code-level trace of one append-loop object:

1. `13-append-loop.sh` invokes `remfield-io write` once per object
   (`fieldtest/scripts/13-append-loop.sh:220-230`), and `remfield-io` performs
   exactly one `open_write_session` → one `append_file` → one
   `close_write_session` per invocation
   (`fieldtest/tools/remfield-io/src/main.rs:174-230`).
2. On close, `close_write_like_critical` unloads whenever the mount has a home
   slot (`crates/remanence-api/src/mount.rs:454`), and
   `resolve_actor_mount_from_library` records a home slot even for an
   already-loaded tape (`mount.rs:839-848`). **Every close physically returns
   the cartridge to its slot**; every next open re-does changer MOVE + drive
   LOAD + thread.
3. A freshly loaded drive legitimately reports `02/04/xx` becoming-ready for
   seconds while it threads/positions, so a fence per object is expected drive
   behavior *given this mount pattern* — on media already past its one-time
   optimization.

This explains every operator observation: a fence on each write and read,
wait-ready succeeding on the first attempt (`attempts=1` means ready at the
first TUR with zero sleep — normal load settle, not calibration), MB/s barely
moving (robot time dominates), and `rem top` showing only `busy` (the drive
*is* busy — loading and unloading).

Two implementation facts bound the production cost of the fence itself:

- A fence row is created **only** when a probe returns not-ready; a ready
  probe returns `Ok(())` with zero database writes
  (`crates/remanence-api/src/write_owner.rs:1128-1175`). A ready tape pays one
  TEST UNIT READY per session open — negligible.
- The wait loop is probe-first: a successful first TUR returns immediately,
  with no sleep before the first probe
  (`crates/remanence-cli/src/lib.rs:6392`, `6743-6898`).

## 3. External facts (verified against primary sources)

- LTO-9 media optimization runs on the **first load only**; average ~40
  minutes, most complete within 60 minutes, up to 2 hours; it must not be
  interrupted; **subsequent loads are normal duration**.
  Sources: Dell KB 000191878 ("New LTO-9 Media May Take Longer than Expected
  to Become Ready"), Quantum LTO-9 Media Calibration FAQ (09/2021), consistent
  with the IBM TS4500 media-optimization page already cited in the design doc.
- Consequence 1: the ~45-minute init window observed on one tape was the
  documented one-time cost. Those four cartridges will never do it again.
- Consequence 2: the fences observed during the append loop **cannot have been
  calibration** — the media was already optimized. They were load-settle
  events amplified by per-object mount churn.
- Consequence 3 (production): optimization should be treated as an **intake
  step**, not a runtime event. Batch-condition new cartridges during idle
  windows so production writes never see a calibration wait.

## 4. Defects and gaps found

1. **Flat 30 s poll defeats the fast ramp.** `media_conditioning_poll_interval`
   (`crates/remanence-cli/src/lib.rs:6704-6711`) yields 15 s polls for the
   first minute then 60 s — but only when the caller passes exactly
   `MEDIA_CONDITIONING_STEADY_POLL` (60 s). The `wait-ready` CLI default and
   the fieldtest wrapper pass `--poll 30s`, which fails the equality test and
   falls into a flat 30 s cadence. A ~10 s load settle therefore costs ~30 s
   wall clock per fence. This is the actual "fence waits too long" quantum.
   Fix: unconditional ramp (Track B, P5).
2. **No per-cartridge "optimization complete" marker.** Optimization is
   one-time, so Remanence should durably record it per cartridge after the
   first successful long conditioning. A *long* becoming-ready on an
   already-optimized cartridge then becomes an anomaly warning, and the marker
   is the correct trigger for the `lto9_media_optimization_possible` display
   hint (Track A).
3. **The production-shaped path has zero physical coverage.** The daemon
   already supports N appends per open session — the drive actor loop
   processes any number of `AppendFinish` commands per session, with readiness
   probed once per open (`write_owner.rs:1857`) — but no field script
   exercises it. The append loop only measures the worst-case
   per-object-session pattern (Track B, P1/P2).
4. **Auto-wait can mask regressions.** Every fence is logged `INFO` and
   retried up to 3×; a 100% fence-per-object ratio had to be reconstructed by
   scrolling. Field scripts need fence accounting and thresholds (Track B,
   P3/P4).
5. **No per-object phase timing in evidence**, so "slow" cannot be attributed
   among load/settle/transfer/filemark/close without manual RCA (Track B, P3).
6. **Physical window priorities.** Still-open layer-5 gates that can *only* be
   proven on real hardware: kill-during-append + restart with the
   uncommitted-tail fence assertion, append-specific rebuild evidence,
   early-warning/EOM behavior. These should outrank cadence tuning for
   remaining MSL3040 time.

## 5. Answers to the brief's eight questions

1. **Repeated fences expected?** Yes, given per-object unload/reload; the
   admission itself never fences a ready tape. Fix the mount churn, not the
   fence.
2. **Keep tape mounted across appends?** Yes. Add a `write-many` field mode
   (one session, N appends) and keep the current per-object cycle as a
   *separate* named mode — it is valuable robot/mount stress. Report the two
   latency profiles separately. Production-side, the standard answer is a
   lazy-dismount timer (Track A).
3. **Immediate follow-up TUR before sleeping?** Already probe-first; the gap
   is the flat 30 s interval after a not-ready first probe. Ramp it (finding
   4.1).
4. **Distinguish readiness sub-states?** Keep the classifier generic
   (TUR-only, family-tagged). Derive *display hints*
   (`load_settle` / `positioning_settle` / `lto9_media_optimization_possible`)
   at the presentation layer from elapsed time, generation, and the
   per-cartridge optimized marker. No new classifier states.
5. **`rem top` fields?** Pull MR-9 forward. Minimum: a session phase enum
   (`loading`, `threading`, `positioning`, `writing`, `reading`,
   `writing-filemark`, `journal-sync`, `unloading`, `readiness-wait`); when in
   `readiness-wait`: operation_id, last sense tuple, elapsed, deadline; plus
   bytes-moved/position so "busy with no MB/s" is self-explaining.
6. **Auto-wait hiding bugs?** Yes as written. Thresholds: warn when a
   readiness wait on already-optimized media exceeds ~90 s; fail beyond
   ~15 min (that is neither settle nor calibration). Every script run should
   emit a fence summary (count, total wait seconds, fences-per-object).
7. **Batching vs append-session API?** Both, at different layers. Daemon side:
   the existing multi-append session plus a lazy-dismount policy amortizes
   mount/readiness/position overhead with no client changes. Sutradhara-side
   batching of small objects remains a policy optimization, not the fix. No
   new API surface until session-reuse + dismount timer proves insufficient.
8. **Acceptance criteria?** Three metrics, never collapsed: (a) streaming
   throughput for large objects in a held-open session (should approach LTO-9
   native, ~300 MB/s class); (b) per-object commit latency in append-session
   mode (dominated by filemark + journal fsync — seconds); (c) full-cycle
   mount→write→dismount latency, reported as a robotics/robustness metric
   only. Correctness invariants (dense tape files, rebuild fidelity, read-back
   at tape file > 1) hold regardless of speed.

## 6. Track A — Remanence design refinements (not field-blocking)

For fold into `lto9-media-readiness-design-v0.1.md` /
`layer5-multi-object-append-design-v0.1.md` and later prompt cuts:

- **A1 Lazy dismount policy.** On session close, keep the cartridge in the
  drive for a configurable idle window (default a few minutes); return to
  home slot on idle expiry, drive contention (another tape needs the drive),
  clean-needed, or daemon shutdown. Interactions to design: readiness
  admission (a kept-mounted tape needs no reload settle — this alone removes
  most production fence events), drive stewardship cleaning windows, startup
  reconciliation of a loaded-but-idle drive, and `rem top` visibility of
  "loaded idle (dismount in Ns)".
- **A2 Per-cartridge optimization-complete marker.** Durable bit set after
  first successful long conditioning (or first successful init on an L9/LZ
  cartridge). Drives: display hint selection, anomaly warning on long
  becoming-ready for optimized media, and intake-conditioning bookkeeping.
- **A3 Intake conditioning workflow.** Batch-condition new cartridges during
  idle windows; with two drives the future `parallel-conditioning` mode halves
  a batch (40 tapes ≈ 13 h parallel vs ≈ 27 h serial at the 40-min average).
  Production writes should never encounter calibration.
- **A4 MR-9 live status.** Prioritize; field list in §5 answer 5. The 45-min
  `Calib` window rendering as generic `busy`/queued is exactly how operators
  lose trust in automation.
- **A5 Poll ramp** (implemented in Track B P5 for field expedience; record the
  design decision here: ramp is unconditional, `--poll` sets the steady-state
  interval only).
- **A6 Benchmark metric split** per §5 answer 8, so no future report quotes a
  single MB/s for an append-stress workload.
- **A7 Streaming write path (found live 2026-07-07: physical bench pinned at
  ~80 MB/s, CPU idle).** `append_object` is store-and-forward: the entire
  object is streamed into a spool **file** first, then `append_finish` reads
  it back and writes tape, strictly serially
  (`crates/remanence-api/src/lib.rs:1975-2100`). The spool dir is hardwired to
  `state_dir/spool` (`crates/remanence-daemon/src/main.rs:76`) — root disk in
  the field layout — so every byte crosses the root disk twice and the two
  phases serialize (effective rate ≤ ½ even with fast media). Not gRPC (local
  unix socket, GB/s class), not block size (256 KiB back-to-back with
  BUFFERED MODE preserved, `tape_io/mod.rs:1416-1429`, can saturate LTO-9).
  Fixes, in order: (1) make `spool_dir` independently configurable — spool
  contents are pre-commit and crash-disposable by design, so tmpfs is
  legitimate; (2) overlap ingest and tape write (bounded ring buffer /
  hash-while-writing) so per-object rate approaches drive rate; the durable
  commit boundary (filemark → journal fsync) is untouched. Field workaround
  needing zero code: symlink `state/spool` → tmpfs
  (`create_private_spool_dir` is symlink-tolerant, `main.rs:141-149`).
  Existing `remanence_write_diag` log lines already decompose spool-phase vs
  append_finish-phase throughput per object.

## 7. Track B — fieldtest improvements (use the physical window)

Cut as one codex prompt set:
`docs/prompt-fieldtest-append-session-evidence.md` (P1 `remfield-io
write-many`, P2 append-loop dual mode, P3 per-object phase timing in evidence,
P4 fence accounting + thresholds, P5 unconditional poll ramp, plus selftest
and cargo-test verification members).

Recommended use of remaining MSL3040 time, in order:

1. Kill-during-append + restart + uncommitted-tail fence evidence
   (physical-only; highest value; needs no new code).
2. Rerun the append loop in both modes once P1/P2 land — separates
   append-format behavior from mount overhead in one afternoon.
3. Forced-seal / early-warning rollover evidence (physical-only).
4. Phase-timing + fence-summary evidence (P3/P4) on every subsequent run makes
   later RCA self-serving.
5. Cadence/live-status improvements are VTL-testable; do not spend library
   time on them.
