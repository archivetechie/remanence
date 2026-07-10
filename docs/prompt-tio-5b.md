# Prompt TIO-5b — pipelined submission, read side + read diag parity + spool orphan reconciliation

**Status:** pending (cut 2026-07-10 from FROZEN design v0.5, st-harvest
folded; dispatch AFTER TIO-5a lands — it builds on the ring/submitter
primitives).
**Normative source — read first and treat as binding:**
`docs/design-tape-io-pipelined-submission-v0.1.md` (v0.5), §§5, 6, 8, 9, 10.
Same frozen constraints as TIO-5a.

## Scope (design §10 item 2)

1. **Read pipeline** (design §5): same ring, reversed — submitter pops an
   empty buffer, issues fixed-mode batch READ (SILI=0; existing
   never-cross-a-filemark clamp and FILEMARK+VALID residual decode
   unchanged), hands the consumer a **typed handoff**
   `{buffer, valid_bytes, records_read, terminal flags}`. Stale tail past
   `valid_bytes` never exposed; fail-closed outcomes (current `read_core`
   behavior) withhold the buffer. The next READ CDB may be staged, but
   **any short/residual outcome (FILEMARK residual, ILI, error) cancels the
   staged CDB**; clamp recomputed from post-decode state before further
   issue. **Fixed-mode ILI additionally invalidates the arithmetic cursor
   (design §5, st-harvest F4)**: handoff withheld, `expected_position`
   cleared, next data command requires explicit reposition + device
   position proof — do NOT copy st's automatic backspace recovery.
   **Read-side classification parity (design §5, st-harvest F1/F2):**
   fixed READ with current RECOVERED ERROR and no terminal flags =
   success delivering only proven-complete records (`valid_bytes` set
   accordingly, audited-as-recovered); reset-UA `06/29/xx` on READ =
   state-invalidating exactly as the write path (stop, invalidate cursor
   + mode validation, re-prove position + MODE before further reads).
2. **Read diag parity** (design §5 — closes field gap #4): restore path
   gains the write path's decomposition — per-phase times (locate/position,
   transfer, relay), per-command cadence histograms (`gap_us`, `ioctl_us`),
   batch effectiveness, bytes/records — in restore diag lines. Goal: a
   field read run decomposes into drive rate vs relay rate vs client write
   rate without guesswork.
3. **Spool orphan reconciliation** (design §6 crash-table row, panel
   finding): `Spool::Drop` only cleans on normal unwinding — SIGKILL
   orphans `spool-*.bin` (on tmpfs: RAM held across crashes, outside the
   budget). Add startup reconciliation BEFORE accepting writes: enumerate
   owned `spool-*.bin` in the configured spool dir, record evidence (names,
   sizes, mtimes) in a diag/log line, remove them, re-account the remaining
   tmpfs budget. Foreign files untouched.
4. **Runbook / fieldtest updates** (design §8): tmpfs requirement + DL385
   dual-drive RAM budget precondition; the five acceptance legs
   (single-drive gap/throughput gate, dual-drive concurrent HBA-decision
   leg with kit-defect-#9 pre-warm, 1 MiB vs 4 MiB `max_sectors_kb`
   comparison, read decomposition, receive-feed-rate counter); effective
   batch + effective mode surfaced in bench output.

## Tests (design §9, read-side + orphan rows)

- typed handoff: sentinel-prefilled ring buffer never leaks stale tail
  bytes past `valid_bytes`;
- FILEMARK residual cancels the staged next CDB (assert absence of the
  boundary-crossing READ); clamp recomputed from post-decode state;
- fixed-mode ILI cursor invalidation (design §9): ILI after N clean
  records and ILI before any record — staged CDB cancelled, handoff
  withheld, `expected_position` cleared, no subsequent data CDB without
  explicit reposition + device position proof (assert no cached-position
  reuse);
- recovered-error READ (design §5): current `0x70/key=1` no terminal
  flags → success, only proven-complete records delivered, `valid_bytes`
  consistent, audited-as-recovered; deferred `0x71/0x73` key=1 →
  completion-unknown always;
- reset-UA on READ (design §5/§9): `06/29/xx` mid-restore → pipeline
  stops, staged CDB cancelled, cursor + mode validation invalidated, no
  further READ without position proof + MODE re-verification;
- fail-closed outcomes withhold the buffer (read_core behavior preserved);
- read batching under pipelining: batch never crosses a tape-file boundary;
  SILI stays 0; read-side MODE SELECT step (TIO-2) unchanged;
- spool orphan reconciliation: SIGKILL leaves orphans → startup enumerates,
  evidences, removes, re-accounts budget before accepting writes; foreign
  files untouched;
- read-side crash rows (design §6) via chaos kill injection;
- read diag fields present and consistent (phase sum ≈ wall; cadence
  histogram populated) in hermetic runs.

## Definition of done (AGENTS.md applies)

Same bar as TIO-5a (workspace tests green, clippy clean, socket tests run —
sandbox limits reported not encoded, no layer-5/commit-ordering changes,
deviations raised not implemented). Summary lists files touched, tests per
invariant, and confirms the design §8 bench/runbook additions are in the
fieldtest kit docs.

**Verification member:** hermetic suite above; after landing, the
`~/system` harness `scenario-append` `covers` gains `rem.tape.pipelined_io`
(system-side, tracked there) and the full suite must run green from a clean
slate (`make suite`). Physical §8 acceptance = next MSL3040 window.
