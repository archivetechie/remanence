# Prompt TIO-6 R2b — diff-gate fixes (1 BLOCKER + hardening + 2 minors + hygiene)

**Status:** pending. **Applies to the R2b working tree on branch `tio6-r2b-wip`** (worktree
`~/remanence-r2b`), which is uncommitted. Objective gate is green (`cargo fmt --check`,
`clippy -D warnings`, **1653 passed / 0 failed / 13 ignored** = QuadStor/VTL-hardware only). A
4-lens Opus diff-gate then found the items below. **Fix these IN PLACE; change nothing else; keep
the single always-on read path (NO mode flag — §11 one-path rule, backout = git revert). Re-run the
full gate. The blocker fix MUST ship with a regression test that fails without the fix.**

## BLOCKER — reservoir park-gate condvar LOST-WAKEUP ⇒ permanent restore deadlock
**File:** `crates/remanence-api/src/read_core.rs`.
**Root cause:** the parked submitter evaluates its wait predicate *while holding* `reservoir.gate`
(the `gate.lock()` at ~L608; predicate `occupancy_bytes > low_bytes` ~L609-610 and `consumer_alive`
~L612), then `wake.wait(guard)` (~L619). But BOTH notifiers mutate the predicate state as lock-free
atomics and call `wake.notify_all()` **without ever acquiring `gate`**:
- `ReservoirState::consume` (~L361-365): `occupancy_bytes.fetch_sub(...)` then `wake.notify_all()`.
- `HandoffBlockSource::drop` (~L280-283): `consumer_alive.store(false)` then `wake.notify_all()`.

`std::sync::Condvar` only guarantees delivery if the state change is serialized against the waiter's
"predicate-checked-true-but-not-yet-suspended" critical section — i.e. the notifier must pass through
the SAME mutex. Failure scenario (permanent hang, drive reservation held):
1. Submitter parked, in `while occupancy > low`, holding `guard`. occupancy = `low + B`.
2. It re-checks `low+B > low` → true, passes `consumer_alive`, is about to `wake.wait(guard)` but has
   not suspended (still holds `gate`).
3. Decode recycles → `consume(B)`: occupancy → `low`, `notify_all()` fires — **submitter not yet
   suspended ⇒ wakeup LOST.** Decode then blocks in `delivery_receiver.recv()` (nothing new issued).
4. Submitter suspends in `wait`. No further `consume`/notify will come. **Both block forever.**
   The dead-peer-while-parked path is the same hole (drop's `consumer_alive=false`+notify lost) —
   violating §4.5 "dead peer while parked ⇒ teardown".

Hermetic `slow_consumer_parks_...` hides it only because 2 ms consumer sleeps dwarf the race window.

**Fix:** make EVERY notifier pass through `gate` before signalling, so it cannot notify during the
waiter's predicate-eval window. After the atomic mutation, acquire-and-release `gate`, then notify:
```rust
// ReservoirState::consume
let previous = self.occupancy_bytes.fetch_sub(bytes, Ordering::AcqRel);
debug_assert!(previous >= bytes);
drop(self.gate.lock().unwrap_or_else(|e| e.into_inner())); // serialize with the waiter
self.wake.notify_all();
```
Apply the identical `drop(self.gate.lock()…)`-before-`notify_all()` guard in `HandoffBlockSource::drop`
around the `consumer_alive.store(false)` + notify. (Zero work is held under `gate`, so no
lock-ordering risk / no throughput cost beyond the microsecond predicate-check window.)

**Regression test (REQUIRED, must fail before the fix):** force the lost-wakeup deterministically —
e.g. a test-only hook/seam that lets the harness release the waiter's `gate` and run `consume`-to-low
(or `drop`) precisely in the "predicate true, not yet suspended" window, then assert the submitter
still makes progress / tears down (bounded, no hang) rather than deadlocking. Add BOTH:
- **die-while-parked (ranged) ⇒ teardown, drive never moves** (§10 row);
- **slow-but-alive ⇒ parked INDEFINITELY, no abort** at the reservoir gate (§10 row — distinct from
  the existing sender-layer `channel_writer_waits_for_slow_but_alive_receiver`).

## HARDENING (fold with the blocker — same region) — make §4.5 hold by CONSTRUCTION
The park wait-loop exit condition (~L609) tests only occupancy; liveness is checked only *inside* the
loop (~L612). In ranged mode a consumer death whose `Drop` recycles a large `pending` queue can
discharge occupancy below low-water *before* it stores `consumer_alive=false`, so the submitter exits
the loop and enters `ResumingProofPending`. On real hardware the resume RP is a READ POSITION *query*
(no motion) and the later frontier `send` fails once the receiver is gone — so it errors out — but
the "drive never moves" guarantee is then delivered by RP *timing*, not construction.
**Fix:** immediately after the `while` wait loop (before the `ResumingProofPending`/resume-RP path,
~L624/631), re-check liveness and fail closed:
```rust
if !reservoir.consumer_alive.load(Ordering::Acquire) {
    return Err(TapeIoError::OperationFailed(
        "read consumer died while reservoir was parked".into()));
}
```
(With the blocker fix serializing `drop` through `gate`, this recheck is race-free.)

## MINOR 1 — ranged proof cadence must clamp to EFFECTIVE reservoir/2 (§5.5)
`run_read_pipeline` uses `config.proof_cadence_bytes` raw (~L795). The only clamp is in
`ReadPipelineConfig::local_default` against the compile-time `DEFAULT_READ_RESERVOIR_BYTES/2`
(~L400-401), NOT the `effective_capacity_bytes` derived after slab-rounding / `MAX_READ_RESERVOIR_SLABS`
/ minimum-pool flooring (~L517-525). Any non-`local_default` caller can then exceed effective/2.
**Fix:** after computing `effective_capacity_bytes`, use a local
`let proof_cadence = config.proof_cadence_bytes.min(effective_capacity_bytes / 2).max(1);` and drive
the ranged-proof cadence from that. (Not a deadlock — occupancy still bounds withholding — but a
spec-compliance gap.)

## MINOR 2 — HandoffBlockSource validation ORDER parity (§3.3)
`HandoffBlockSource::install_next` (~L190-202) checks byte/record mismatch FIRST, then
filemark/zero-record; the deleted `BatchingBlockSource::refill` checked filemark/zero-record FIRST.
For a funnel-inconsistent handoff (e.g. `records_read == 0` with `valid_bytes != 0`) the error strings
differ — not byte-for-byte parity (never bites today because the funnel guarantees consistency).
**Fix:** reorder `install_next` to test `filemark || records_read == 0` before the byte/record
mismatch, matching the original precedence. Keep `handoff_block_source_validation_errors_preserve_refill_wording`
comparing against the literal original wording (do not weaken it to a tautology).

## HYGIENE — explicit `munlock` on `LockedSlabPermit::drop`
For small-block tapes where `batch_bytes` is below glibc's mmap threshold (heap-backed buffers),
freeing a `ReadBuffer` does NOT release the mlock on reused heap pages, slowly consuming
`RLIMIT_MEMLOCK` across many restore windows. Worst case = eventual minimum-pool mlock failure ⇒
refuse-to-start (fails safe, never OOM/swap). **Fix:** `munlock` the slab in `LockedSlabPermit::drop`,
paired with the buffer's address/len, before the permit's reservation rollback.

## Definition of done (AGENTS.md)
`cargo test --workspace` green (now including the new regression rows), `cargo fmt --check`,
`clippy --all-targets -- -D warnings`. Confirm the deadlock regression test FAILS on the pre-fix code
and PASSES after (state this in the summary). No new mode flag; single always-on read path preserved;
`validate_residual_claim` remains the sole residual funnel (do not fork). Summary: files touched, each
fix → its test, and the pre-fix-fails/post-fix-passes confirmation for the blocker.
