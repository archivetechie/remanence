# Prompt TIO-5a-fixes — flag removal + diff-gate findings on the pipelined write path

**Status:** pending (cut 2026-07-10 from the TIO-5a diff gate; REVISED same
day for the v0.6 owner decision — the runtime backout flag is removed).
**Work ON the branch `tio-5a-wip-20260710`** (do NOT merge to main; do not
rebase). **Normative:** `docs/design-tape-io-pipelined-submission-v0.1.md`
(**FROZEN v0.6** — read the v0.6 status block and §§6.2/7 first; they
changed after the code on this branch was written) and
`docs/prompt-tio-5a.md`. Findings are verified against the code — fix root
causes. Every correctness fix ships a regression test reproducing its
failure scenario.

## Fix 0 — remove ALL mode switches; exactly one tape I/O path (design v0.6+v0.7)

Delete `tape_io.pipelined_submission` AND `tape_io.legacy_single_block`
(config keys, plumbing, every mode branch). The pipelined path IS the tape
I/O path:

- Delete the non-pipelined batched transfer path and its machinery
  (`StagedBlockSink` and friends) AND the legacy serial single-block mode
  plumbing (the variable-mode per-block-RP branch). Single-record
  *operations* used by identity/readiness probes stay — what dies is the
  legacy *mode*, not the ability to read one block.
- Config parsing REJECTS the two removed keys with a message naming the
  removal — a stale config must fail loudly, never silently change
  meaning.
- `PipelinedStagedBlockSink` (rename appropriately) is now the only
  sink — this subsumes diff-gate finding 9 (the near-verbatim copies die
  with the old sink).
- Replace the ON/OFF equivalence tests with **golden fixtures captured
  from main** (design §6.2 v0.6): check in the canonical write/read
  command-stream + timeout-class fixtures generated from main
  (git worktree of main, or committed fixture files produced by a
  fixture-gen test on main — state in your summary how you generated
  them), and assert the pipelined path reproduces them modulo the named
  designed deltas (900 s timeout values). Cross-version stored-image
  tests stay.
- Status/diag drops the mode field entirely (one path); keep the ring and
  cadence diag.

## Correctness fixes (all mandatory)

1. **Fence every transfer error class** —
   `crates/remanence-api/src/pool_write.rs:2935` (+ no-parity variant
   ~:3100, :2155): with the flag gone the `if !pipelined_submission` gate
   dies, but the pipelined drain must still route EVERY transfer error
   into `record_tape_io_fence_for_transfer_error` — producer-side errors
   (source read I/O, parity/format), SPACE(EOD) failures, and Position
   failures included, exactly as TIO-3/4 fenced every transfer error.
   Test: producer fails after ≥1 committed window ⇒ fence row present;
   same for space_to_end_of_data and position failures.

2. **Never let ring plumbing mask a device error** — `pool_write.rs:2208`
   (`execute_pipelined_window`) and :2289
   (`finish_pipelined_window_failure`): `return_ring_buffer(...)?` runs
   BEFORE the write result is examined; a disconnected free ring discards
   a fence-worthy device failure and its deferred audit. Fix: evaluate and
   record the device outcome first; buffer-return failure is secondary
   (log/attach, never replace). Test: producer drops sink mid-window +
   in-flight WRITE partial-fails ⇒ fence row records the tape error,
   deferred sense audited, surfaced error names the tape failure.

3. **Audit emission must not depend on fence success** —
   `pool_write.rs:2291` and the WriteFilemarks drain arm (:2142-2145):
   `on_safety_error(&error)?` returns before the deferred pipeline audit
   flushes. Design rule: fence-before-audit is an ORDERING, not a
   dependency — audit evidence flushes even when the fence write fails
   (both failures surfaced). Test: fence callback errors ⇒ deferred audit
   still flushed, both errors reported.

4. **Make reset-UA recovery reachable** —
   `crates/remanence-library/src/handle/tape_io/mod.rs:2069`:
   `mode_reverification_required` can never clear on the write actor's
   daemon-lifetime DriveHandle — `prepare_drive_for_write`
   (write_owner.rs:2151) runs `verify_tape_identity` (read gated by the
   flag) BEFORE `read_config`, and `write_config` refuses revalidation
   while the flag is set. Fix per design §3.2: when the flag is set, the
   write-prepare path performs MODE SENSE re-verification of the
   tape-sourced block size (and re-proves position) FIRST, clearing the
   flag on success. Test: reset-UA mid-session ⇒ next prepare on the same
   handle succeeds after re-verification; no daemon restart.

5. **Scope the read-side state gate correctly** — `tape_io/mod.rs:1258`
   (`read_block_batch`) and :1406 (`read_block`): the
   `ensure_position_known_for_write` gate fires for ALL completion-unknown
   classes — broader than design §5's mandate (state-invalidating
   reset-UA / mode-invalidated only). Scope it to the state-invalidating
   class; rename the helper so it no longer claims "for_write". Test:
   reads after a non-reset completion-unknown event behave as TIO-3/4
   did; reads after reset-UA refuse until position + MODE re-proof.

6. **Preserve the WRITE's sense when arbitration RP fails** —
   `tape_io/mod.rs:1162` (`write_block_batch_pipelined` EOM path):
   `self.read_position_pipelined()?` drops the original WRITE's non-GOOD
   sense — no TapeWrite audit, fence misclassified as `transfer_error`
   instead of `partial_batch`. Fix: on RP failure, still audit the WRITE
   with its sense, classify the fence from the WRITE outcome, attach the
   RP failure as secondary. Test: EOM WRITE failure + injected RP failure
   ⇒ WRITE audit present, fence reason correct, RP error attached.

## Cleanups

7. `pool_write.rs:2100` — the ring-accounting imbalance check must not
   replace a real transfer error; attach, don't overwrite.
8. `tape_io/mod.rs:2182` — `read_position_pipelined_expected` must wrap
   `read_position_inline_with_cdb` (:2268), not re-implement its
   execute/parse/record core. Same policy for the :1162 vs :969/:1798
   READ POSITION split — one helper, one audit policy, used everywhere.

## Structural invariants (binding)

- **Single safety funnel, by construction:** after Fix 0 there is exactly
  ONE tape I/O path; every error class terminates in the one
  fence-recording funnel. Acceptance is grep-level: no mode flags exist;
  no safety call site is conditioned on any mode; `record_tape_io_fence*`
  reachable from exactly one funnel.
- **Golden-baseline fixtures** are generated from main, never from this
  branch's own behavior.
- **Wrap, don't copy:** a near-verbatim copy of an existing helper is a
  defect even with green tests.

## Definition of done

AGENTS.md applies: `cargo test --workspace` green, `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, socket tests run (sandbox
limits reported, never `#[ignore]`d), commit per green milestone ON THE
BRANCH. No design deviations — raise instead of deviating. Summary lists
each fix # → commit → regression test name, and how the golden fixtures
were generated.
