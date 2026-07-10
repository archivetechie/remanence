# Prompt TIO-5a-fixes — diff-gate findings on the pipelined write path

**Status:** pending (cut 2026-07-10 from the TIO-5a diff gate — 6 CONFIRMED
correctness findings + 4 cleanups; branch `tio-5a-wip-20260710`, base commit
`603a921`).
**Work ON the branch `tio-5a-wip-20260710`** (do NOT merge to main; do not
rebase). **Normative:** `docs/design-tape-io-pipelined-submission-v0.1.md`
(FROZEN v0.5) and `docs/prompt-tio-5a.md`. The findings below are verified
against the code — fix root causes, not symptoms. Every correctness fix
ships a regression test reproducing its failure scenario.

## Correctness fixes (all mandatory)

1. **Fence every pipelined transfer error** —
   `crates/remanence-api/src/pool_write.rs:2935` (+ the no-parity variant
   ~:3100, and :2155): the outer error path is gated on
   `if !pipelined_submission`, so producer-side errors (source read I/O,
   parity/format) and SPACE(EOD)/Position failures inside the drain never
   call `record_tape_io_fence_for_transfer_error`, while TIO-3/4 fenced
   every transfer error. Fix: pipelined transfers must reach the same
   fence-recording path for EVERY transfer error class, not only
   window/filemark sink failures via `on_safety_error`. Test: producer
   fails after ≥1 committed window ⇒ fence row present; same for
   space_to_end_of_data and position failures.

2. **Never let ring plumbing mask a device error** —
   `pool_write.rs:2208` (`execute_pipelined_window`) and :2289
   (`finish_pipelined_window_failure`): `return_ring_buffer(...)?` runs
   BEFORE the write result is examined, so a disconnected free ring
   (producer already exited) discards a fence-worthy device failure and
   its deferred audit, surfacing a channel-plumbing error instead. Fix:
   evaluate/record the device outcome first; treat buffer-return failure
   as secondary (log/attach, never replace). Test: producer drops sink
   mid-window + in-flight WRITE partial-fails ⇒ fence row with the tape
   error, deferred sense audited, surfaced error names the tape failure.

3. **Audit emission must not depend on fence success** —
   `pool_write.rs:2291` and the WriteFilemarks drain arm (:2142-2145):
   `on_safety_error(&error)?` returns before the deferred pipeline audit
   flushes, so a fence-write failure (catalog DB error) loses the SCSI
   error audit entirely. Design rule: fence-before-audit is an ORDERING,
   not a dependency — audit evidence must flush even when the fence write
   fails (both failures surfaced). Fix + test: fence callback errors ⇒
   deferred audit still flushed, both errors reported.

4. **Make reset-UA recovery reachable** —
   `crates/remanence-library/src/handle/tape_io/mod.rs:2069`:
   `mode_reverification_required` can never clear on the write actor's
   daemon-lifetime DriveHandle — `prepare_drive_for_write`
   (write_owner.rs:2151) runs `verify_tape_identity` (read gated by the
   flag) BEFORE `read_config`, and `write_config` refuses revalidation
   while the flag is set. Fix per design §3.2 state-invalidating recovery:
   the write-prepare path must perform MODE SENSE re-verification of the
   tape-sourced block size (and re-prove position) as the FIRST step when
   the flag is set, clearing it on success — before any read-dependent
   step. Test: reset-UA mid-session ⇒ next prepare on the same handle
   succeeds after re-verification; no daemon restart required.

5. **Restore OFF-path read equivalence** — `tape_io/mod.rs:1258`
   (`read_block_batch`) and :1406 (`read_block`): the
   `ensure_position_known_for_write` gate was added unconditionally —
   a flag-independent behavior change to the `pipelined_submission=false`
   path (contract: exact TIO-3/4 reproduction) and broader than the
   design's reset-UA-only read-side mandate (F1). Fix: scope the read-side
   gate to the state-invalidating class (reset-UA / mode-invalidated),
   applied identically in both modes only where design §5 mandates it;
   OFF-path behavior for other completion-unknown classes reverts to
   TIO-3/4. Rename the helper so its name no longer claims "for_write".
   Test: OFF-path equivalence extended to cover reads after a
   non-reset completion-unknown event.

6. **Preserve the WRITE's sense when arbitration RP fails** —
   `tape_io/mod.rs:1162` (`write_block_batch_pipelined` EOM path):
   `self.read_position_pipelined()?` drops the original WRITE's non-GOOD
   sense — no TapeWrite audit, fence misclassified as `transfer_error`
   instead of `partial_batch`. Fix: on RP failure, still audit the WRITE
   with its sense, classify the fence from the WRITE outcome
   (completion-unknown/partial), and attach the RP failure as secondary.
   Test: EOM WRITE failure + injected RP failure ⇒ WRITE audit present,
   fence reason `partial_batch`/completion-unknown, RP error attached.

## Cleanups (mandatory unless a fix above already removes the code)

7. `tape_io/mod.rs:1162` vs :969/:1798 — EW/EOM classification uses
   audited `read_position_pipelined()` on the ON path but unaudited
   `read_position_inline()` on OFF/write_filemarks: unify so the ON-vs-OFF
   normalized audit trace does not diverge for the EW/EOM class.
8. `pool_write.rs:2100` — the ring-accounting imbalance check must not
   replace a real transfer error; attach, don't overwrite.
9. `pool_write.rs:1791` — deduplicate
   `PipelinedStagedBlockSink::{check_poison, request, seed_cursor,
   advance_cursor}` against `StagedBlockSink` (:1473+); one behavioral
   drift already exists — eliminate it via shared code.
10. `tape_io/mod.rs:2182` — `read_position_pipelined_expected` must wrap
    `read_position_inline_with_cdb` (:2268), not re-implement its
    execute/parse/record core.

## Definition of done

AGENTS.md applies: `cargo test --workspace` green, `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, socket tests run (sandbox
limits reported, never `#[ignore]`d), commit per green milestone ON THE
BRANCH. No design deviations — raise instead of deviating. Summary lists
each finding # → fix commit → regression test name.
