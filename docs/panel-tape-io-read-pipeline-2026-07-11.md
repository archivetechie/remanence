# TIO-6 panel findings checkpoint (fold basis)

Design: docs/design-tape-io-read-pipeline-v0.1.md. Lenses: throughput/cost, SCSI, concurrency (in); failure-modes (pending rerun a7cdc3b0248ba1703).

## CONVERGENT (2+ lenses → high confidence)
- **[BLOCKER/MAJOR] Decode-thread must hold ZERO drive access.** SCSI-B1 + Concurrency-#3 independently. The "HandoffBlockSource = BatchingBlockSource with refill's SCSI swapped for recv(), everything else byte-for-byte" recipe (§3.3) retains `inner:&mut dyn BlockSource` (live drive) on the decode thread → concurrent execute_in on same sg fd = corruption; and one-in-flight becomes convention not construction. FIX: decode-side source is a DISTINCT type holding only {delivery-recv, block_size, remaining slaved to Σ handoff.records_read, validation checks}, NO drive ref; drive BlockSource MOVED into submitter closure; give the format consumer a narrow read-only sub-trait so motion methods are absent BY TYPE. Delete "reuse BatchingBlockSource verbatim." (§5.4 "compile-time removal" also inaccurate — trait methods are required; best is unreachable!() runtime panic.)
- **[MAJOR] Residual trusted for valid_bytes with NO bytes_transferred cross-check.** SCSI-M2 (+ my task #6). `bytes_transferred` destructured at mod.rs:1400-1402 then discarded; valid_bytes = records_read*block_size from sense residual only. Under ring REUSE, over-reported residual exposes PRIOR window's real tape data (worse than zero/sentinel). FIX: require records_read*block_size <= bytes_transferred, fail-closed on disagreement + hermetic test. Design-wide (write too) but read exposure acute.
- **[MAJOR] Per-object positioning unmodeled (access-pole workload).** Throughput-MAJOR-2 + Concurrency-#8. §7 is single-large-object; production = "pull N clips"; rewind/BOT-verify/SPACE per object (§3.4 "as today") + per-object spawn/join/drain → positioning dominates (ate the WRITE field number, ~65s/object). FIX: state verify+SPACE is per-mount not per-object; add multi-object acceptance leg (§10); higher-value arc may be amortizing inter-object positioning, not the transfer submitter.

## THROUGHPUT/COST lens
- **[MAJOR-1] Gate R2 on measurement — don't pre-commit.** R1+sync ceiling 210-250 (§2-D) ≈ the ≥250 gate; on 1GbE (~110 cap) R2 buys ZERO; R2 win is ~20% headroom on 10GbE+ only. FIX: R2 dispatch CONDITIONAL on leg-0 measuring R1+sync below gate on real production path AND confirming 10GbE+. Ship R1, measure, then decide. (Cost-efficiency + resolves measure-vs-build.)
- **[MAJOR-3] Decode thread serial (copy+parse+SHA); budget hinges on SHA-NI.** If no SHA-NI, SHA alone 300-500 brackets 300. FIX: confirm SHA-NI BEFORE R2 freeze (DL385=EPYC, has SHA ext since Zen1; `openssl speed sha256` = 5 min). Keep SHA inline (Q2 correct).
- **[MAJOR-4] "Accept shoe-shine" (§4) dismisses cheap RAM mitigation on wrong grounds.** Buffering doesn't raise sustained throughput (true) but DOES cut reposition FREQUENCY = wear. A few hundred MB owned RAM coarsens reposition granularity. "Restores rarer than ingests" questionable for daily-production access pole (1GbE newsroom restore shoe-shines constantly). FIX: leg-3 measure reposition WITH deep host buffer vs without; price wear against RAM option not restore-to-spool strawman. → sharpens owner Q1.
- **[MAJOR-5] Read cadence presented as fact in §2-A/§3.1 but §7.2 calls it biggest unmeasured term.** Write→read symmetry assumption; on reads drive must stream AHEAD; host stall drains drive buffer → next READ waits on TAPE not µs. FIX: inline the §7.2/Q7 caveat into §2-A/§3.1.
- **[MINOR] 4KiB ×64 is fixed-per-record (~7.5% core at 300), and it's a TAPE GEOMETRY (AOX034 4KiB-init) penalizing EVERY restore off that tape, not just small objects.** FIX: micro-bench before R2 freeze; fold coalesced copy-out into R2 iff it binds (Q6 = "in R2 iff bench binds"). [corrects my layman Q6]
- **[MINOR] Acceptance gate single-object best-sink; passing ≠ real restore throughput.** FIX: add multi-object + production-network-tier legs.

## SCSI lens
- **[MAJOR M3] Ranged reads have NO integrity verification; read-ahead widens deliver-before-position-proven window.** read_plaintext_file_range streams raw, no hash, only 1GiB arithmetic tripwire; read-ahead delivers ~4MiB (ring) before next tripwire proves cursor. FIX: per-range/chunk digest from RAO manifest OR bound deliver-ahead past unproven position for hash-less reads; add to §5.3.
- **[MAJOR M4] Recovered-error + VALID residual + NO terminal flag = physically impossible residual, trusted as short instead of fail-closed (st O7).** Advances cursor BEHIND physical tape → read-ahead delivers wrong-position data. FIX: on recovered-no-terminal-flags require residual==0; VALID nonzero residual ⇒ completion-unknown/poison; keep unwrap_or(records) only for VALID-unset. Add fail-closed test.
- **[MINOR m5] byte-identical CDB stream claim holds only on all-GOOD full-batch object** (residual/recovered/filemark → next count runtime-derived). Name the precondition.
- **[MINOR m6] is_reset_unit_attention covers 06/29/00..04 only, not 29/07 (I_T NEXUS LOSS)** — equally state-invalidating, falls through. Pre-existing.
- **[NIT n7] §5.1 row 2 mislabels common recovered case as "short" (it's full).**
- AFFIRMED SOUND: staged-READ-cancel-on-residual (host memory only, never issued, can't race/corrupt); ILI cursor invalidation genuinely enforced.

## CONCURRENCY lens
- **[MAJOR #1] "next count already in hand" + "armed intent survives as-is" (§3.1/§5.2) can over-read past plan tail.** Carrying count across full outcome without re-clamp → last batch requests `batch` not `remaining` → spurious fail-closed on tail batches + violates §9.2. Backstop: funnel dual clamp iff submitter passes fresh remaining. FIX: count recomputed min(batch, remaining_after_decode) before EVERY issue; only BUFFER carried; delete "survives as-is"; §10 assertion requested_records ≤ remaining_after_decode.
- **[MAJOR #2] "swap one call in refill" not achievable — refill conflates clamp/alloc/issue/validation; hides plan-state divergence** (decode side second `remaining` copy). FIX: decode-side `remaining` only decremented by handoff.records_read, never recomputed. (⊂ convergent #1)
- **[MAJOR #4] decode→sender channel depth unspecified; depth-2 rendezvous re-couples SHA to network, undercuts split.** §6.4 resizes sender→tonic but not decode→sender. FIX: size decode→sender in bytes, state depth; Q4 answer contingent on it.
- **[MAJOR #5 — TOUCHES R1 (dispatched!)] "blocking_send + 30s watchdog" not cleanly implementable; drops client-stall deadline → stalled client pins drive for minutes (regression).** tokio blocking_send has no timeout/can't be interrupted. FIX: async sender with send_timeout (block_on from OS thread) OR watchdog drops receiver; blocking_send panics on runtime worker → must be OS thread/spawn_blocking. **→ R1 DIFF-GATE MUST verify the 30s deadline is preserved via a sound mechanism (BrokenPipe/TimedOut semantics intact), sender on OS thread, no drive-hold regression.**
- **[MINOR] terminal-status uniqueness under concurrent poison sources unspecified** (sender sole emitter, once).
- **[MINOR] cross-thread diag counters need atomics or post-join snapshot** (3 writers now).
- **[NIT] non-blocking-push proof unguarded vs buffer-count/capacity mismatch** — add construction assertion seeded_buffers==ring==delivery_cap.
- POSITIVES VERIFIED: no deadlock (line graph); push-never-blocks correct; buffer aliasing prevented by Rust move.

## FAILURE-MODES lens (4th, in)
- **[BLOCKER] "wrap read_block_batch unmodified" contradicted by audit-coalescing (§5/§8/§9.2).** read_block_batch fires per-command fire_tape_started/finish_tape_success (1359/1392); §8/§3.5 want them REPLACED by a coalesced window span → impossible without modifying the funnel or forking an audit-free read_block_batch_pipelined (the write side DID fork = ~200-line dup = TIO-5a's 6 defects). FIX (clean): audit hook is None/unwired in prod (ns) → DROP/DEFER coalescing, wrap truly unmodified (keep per-command events). If forensic coalescing wanted, extract audit-free CORE called by both, never a fork. (⊂ convergent #1 — 3rd lens on wrap-vs-copy.)
- **[MAJOR] read_plaintext_file_range is a 2nd synchronous consumer (§3.4/§10), neither pipelined nor exempted → "one path" false.** FIX: rework onto HandoffBlockSource+decode/sender OR declare ranged a 2nd path + drop one-path claim. (⊂ SCSI-M3.)
- **[MAJOR] error precedence across 3 threads unspecified (§3.3/§9) → client gets derived "channel closed" not real SCSI cause, gutting attributability.** FIX: submitter TapeIoError (root) > decode-derived > sender, except genuine client disconnect; delivery channel carries Result<ReadBufferHandoff, TapeIoError>.
- **[MAJOR — R1] "30s watchdog around blocking_send" underspecified (§6.4.1).** tokio blocking_send no timeout/uncancellable. FIX: Handle::block_on(timeout(dur, send)) or reserve-with-timeout; specify thread/runtime (block_on on runtime worker fragile). (⊂ Concurrency-#5 — 2nd lens on R1.)
- **[MAJOR — R1] 30s send-abort CONTRADICTS §4 accept-slow-consumers.** slow-but-alive 1GbE client pausing >30s (GC/flush) gets KILLED; dead peer invisible to send-stall anyway. liveness≠slowness. FIX: detect death via tonic/h2 conn state (or much longer deadline), let slow-alive run. **→ R1 deadline policy depends on owner Q1 (shoe-shine).**
- **[MAJOR — R1] zero-copy refcounted slices (§6.4.4) BREAKS ring/network decoupling → slow client pins ring buffers → early shoe-shine. NEGATIVE value.** FIX: DROP item 4 from R1; keep copy-out.
- **[MAJOR] §5.1 omits GOOD-batch+inline-tripwire-RP-fail row** (stale-staged-after-good case; behavior correct fail-closed but table must carry it).
- **[MINOR] gap_us absorbs free_wait (double-count); asymmetric stop detection (free-recv-disconnect); channel sizing/drop-order under-spec; R2-without-R1 shoe-shines EVERY restore (drive health, R1 is operational prereq not just measurement); shoe-shine only observable at session close (add live signal).**
- **[NIT] cadence glosses periodic inline tripwire RP; no mid-object restore resume (name it).**
- AFFIRMED SOLID: buffer aliasing impossible (Rust move); staged-cancel sound; no deadlock/credit-loop/lost-wakeup.

## FINAL CONVERGENCE MAP
- **wrap-vs-copy / decode-thread-no-drive / drop-coalescing**: SCSI-B1 + Concurrency-#2/#3 + Failure-BLOCKER (3 lenses) → TOP fold.
- **R1 blocking_send + deadline-vs-slow-consumer + drop-item-4**: Concurrency-#5 + Failure ×3 (2 lenses) → R1 re-spec, deadline depends on Q1.
- **residual/bytes_transferred cross-check + impossible-residual fail-closed**: SCSI-M2/M4 + task #6.
- **ranged-read integrity + 2nd path**: SCSI-M3 + Failure-major.
- **per-object positioning / multi-object leg**: Throughput-M2 + Concurrency-#8.
- **gate R2 on measurement**: Throughput-M1 + Failure (R2 op-prereq on R1).

## R1 STATUS (codex running, worktree ~/remanence-r1, branch tio6-r1-relay-wip)
Concurrency #5 is the key R1 diff-gate item: verify deadline-preserving mechanism, not naive blocking_send. My R1 prompt required "preserve the deadline, same error semantics" so the requirement is stated; diff-gate verifies codex's mechanism.

## OPEN-QUESTION status (for consolidated report)
- Q1 shoe-shine: now weigh vs cheap RAM-buffer mitigation (throughput-MAJOR-4), NOT restore-to-spool. Business.
- Q2 SHA inline: CONFIRMED right; de-risk by confirming SHA-NI (throughput-MAJOR-3). 
- Q3 client-flag ownership: still business (who owns client-side gRPC flags).
- Q6 4KiB: it's a tape geometry not small-object; in-R2-iff-microbench-binds.
- R2-vs-R1 (new, from throughput-MAJOR-1): the big one — gate R2 on measurement. Business-ish (spend decision).

---

## VERIFY ROUND (codex gpt-5.6-sol, 2026-07-11 09:10) — FAIL (8 majors + 2 minors)

codex
FAIL — the design does not meet the freeze bar. No blocker remains, but I found six majors, including two panel majors that are not actually resolved.

## Findings

1. **major × §5.5 / §3.2 × ranged-read proof cannot reach the decoder**

   The delivery type remains `Result<ReadBufferHandoff, TapeIoError>`, but `ReadBufferHandoff` carries only bytes, record count, and terminal flags; it discards `ReadBatchOutcome::position_evidence`. Therefore the decoder cannot know which handoff completed a device-proven tripwire and cannot advance the release frontier described in §5.5. See [design §3.2](/home/user/remanence/docs/design-tape-io-read-pipeline-v0.1.md:260) and [ReadBufferHandoff](/home/user/remanence/crates/remanence-library/src/handle/tape_io/model.rs:511).

   The document also omits where the withheld ≤256 MiB is stored and charged. Returning those buffers before proof would expose/reuse them; retaining them requires a bounded pending-proof queue included in reservoir occupancy and RAM accounting.

   **Claim:** Prior SCSI M3 and the second-path major are resolved.

   **Fix:** Extend the delivery protocol with ordered proof-frontier metadata, such as a handoff sequence/end cursor plus optional `DevicePositionProof`, preserving proof evidence from `ReadBatchOutcome`. Specify a bounded pending-proof queue, count it as reservoir occupancy, release only through a matching proof frontier, and test proof failure with buffered output. Transport unification itself is resolved; integrity is not.

2. **major × §5.6 × a once-cell cannot enforce the stated error precedence**

   A first-writer-wins once-cell cannot guarantee `submitter > decode > sender`: a sender failure can fill it before the later SCSI root arrives. “Highest-precedence via a once-cell” is internally contradictory. See [design §5.6](/home/user/remanence/docs/design-tape-io-read-pipeline-v0.1.md:792).

   **Claim:** The real SCSI cause always reaches the client.

   **Fix:** Use a replaceable priority-ranked terminal accumulator, and do not emit terminal gRPC status until all upstream producers have closed/joined or an explicit finalization barrier proves no higher-priority cause can still arrive. Preserve genuine disconnect as the separate short-circuit case.

3. **major × §4.2 × consumer EWMA does not prove the drive is speed-matched or guarantee zero parks**

   Consumer drain rate is only a demand proxy. It does not reveal drive-buffer state, actual tape speed band, backhitches, or whether the drive has already parked. Consequently, “demand converges to consumer rate → hardware speed-matching → zero parks” is a hypothesis, not a sound control invariant. A bursty consumer can remain above the EWMA floor while still presenting gaps long enough to stop the drive.

   The `high−ε` controller also leaves ε undefined relative to one command, watermark overshoot, sample period, EWMA time constant, and minimum dwell time. The ±10%/10-second selector prevents noise flapping near a steady floor, but not repeated transitions under bursty workloads.

   **Claim:** One rate comparison safely selects in-band versus sub-band control without thrash.

   **Fix:** Define ε as at least one maximum handoff/command credit; specify EWMA sampling and time constant, minimum regime dwell, and transition state. Treat “in-band” as provisional and use observed park/backhitch or drive-buffer/LOG SENSE evidence as negative feedback: any park under in-band control demotes to full-hysteresis batching. Remove the unconditional “zero parks” claim until leg 3 proves it.

4. **major × §4.3 × `T_reproof = 250 ms` is an unqualified proxy for parking**

   The safety rule says re-proof after a pause “in which the drive may have parked,” but implements that as pause duration greater than 250 ms. No evidence establishes that this drive cannot park or reposition during a shorter pause. Therefore a legitimate sub-250-ms stop can issue its first subsequent READ without the promised proof. See [design §4.3](/home/user/remanence/docs/design-tape-io-read-pipeline-v0.1.md:519).

   Filemark handling itself is coherent: gating is at classified command boundaries, plan-bounded windows do not intentionally cross object/filemark boundaries, and an unexpected mid-plan filemark terminates the window.

   **Claim:** The first post-park READ is always gated by `DevicePositionProof`.

   **Fix:** Re-proof after every deliberate watermark park/resume regardless of duration. Use `T_reproof` only for incidental free-buffer waits, and either qualify it physically or conservatively re-proof every such resumed wait. Make the RP result/proof an explicit precondition before issuing the next READ.

5. **major × §4.5 × dead-peer teardown has no specified bounded implementation**

   The design says keepalive values are in `reference-configuration.md`, but that file contains only h2 flow-control windows—no keepalive interval, timeout, or idle behavior. The current server builder likewise configures windows only. See [reference configuration](/home/user/remanence/docs/reference-configuration.md:73) and [server builder](/home/user/remanence/crates/remanence-daemon/src/lib.rs:40).

   Thus the claimed interval+timeout bound is undefined. As folded, a half-open peer can still leave a parked drive reserved forever—the failure §4.5 is intended to eliminate.

   **Claim:** Dead and half-open peers are always detected independently of send progress.

   **Fix:** Specify concrete interval/timeout defaults, whether PING runs while otherwise idle/stalled, and the exact tonic server/client settings. Include the values in configuration documentation and test a half-open connection while the reservoir is parked. Slow but transport-alive indefinite parking is an explicit owner decision and is internally coherent once death detection is bounded.

6. **major × §4.6 × RAM rules do not justify “never OOM, never swap”**

   `MemAvailable` is a transient observation, not a reservation. Two streams can both observe the same availability unless capacity is reserved atomically before either grows. The document mentions a generalized shared ceiling but does not define its fixed size, reservation order, permit granularity, or how memlock allowance is atomically shared.

   “If `mlock` fails, clamp to the minimum pool” also contradicts “resident, non-swappable”: an `mlock` failure does not establish that even the minimum pool can be locked. Incremental `MemAvailable` checks cannot prevent unrelated processes from consuming memory after the check.

   **Claim:** The math cannot OOM or swap, including two restores plus append spool.

   **Fix:** Define a fixed daemon I/O-memory ceiling below a cgroup/systemd memory limit and an atomic reservation manager shared by spool and all drives. Reserve locked-byte permits before allocation; rollback permits on allocation or `mlock` failure. Refuse or explicitly permit only the legacy minimum swappable ring if minimum locking fails—do not call it “never swap.” Include concurrent growth races, not merely second-stream startup, in tests.

7. **major × §4.1 / §4.8 × cycle arithmetic contradicts the defaults and understates wear**

   The design correctly states cycles ≈ `restore_bytes ÷ (high−low)`, then claims a 100 GB reservoir gives about 180 cycles for 18 TB. With 90/25 watermarks, the span is 65 GB, giving roughly 277 cycles, not 180. With the 4 GiB default, the span is 2.6 GiB and a full 18 TiB restore is roughly 7,100 park/resume cycles.

   At a 110 MiB/s consumer, draining that 2.6 GiB span takes only about 24 seconds, so the default can generate around 150 cycles/hour. That is not demonstrated to meet the anti-wear goal.

   **Claim:** The 4 GiB, 90/25 defaults are sane and the stated wear estimate supports them.

   **Fix:** Correct all arithmetic using effective `(high−low)` bytes; define an acceptable maximum cycles/hour or cycles/full-tape target; derive the default reservoir from that target and qualified floor. Until physical qualification, label 4 GiB as an experimental minimum, not a sane anti-wear default.

8. **major × §4.4 / §5 × “one shared funnel fixes reads and writes” is false and write compatibility is underspecified**

   The read hardening is correctly placed in `read_block_batch`, but reads and writes do not share that funnel. There are also two distinct write implementations: `write_block_batch` and `write_block_batch_pipelined`. Their recovered/EOM arbitration differs. See [ordinary write funnel](/home/user/remanence/crates/remanence-library/src/handle/tape_io/mod.rs:958), [pipelined write funnel](/home/user/remanence/crates/remanence-library/src/handle/tape_io/mod.rs:1119), and [read funnel](/home/user/remanence/crates/remanence-library/src/handle/tape_io/mod.rs:1324).

   The fold merely says “symmetric check in the same commit”; it does not identify applicable write branches or prove that `bytes_transferred` has the required data-out semantics. A naive check could conflict with the write path’s device-position arbiter.

   **Claim:** The hardening benefits reads and writes without changing existing write behavior.

   **Fix:** Keep the read checks in the single read funnel. Separately specify a shared validation helper and enumerate its call sites in both write functions. State which residual-derived write outcomes it governs, verify transport `bytes_transferred` semantics for data-out CHECK CONDITION, and preserve device-position arbitration for EOM/EW. Add parity tests for ordinary and pipelined writes.

9. **minor × §3.1 / §5.2 × submitter remaining is mislabeled as “post-decode”**

   The submitter decrements its authoritative count after funnel completion, while decode runs asynchronously and may lag by reservoir depth. Therefore `remaining_after_decode` is not the state used to issue the next READ. See [design §3.1](/home/user/remanence/docs/design-tape-io-read-pipeline-v0.1.md:188).

   **Fix:** Rename this consistently to `remaining_after_classified_completion` or `submitter_remaining`; keep the decoder’s sum explicitly diagnostic/derived.

10. **minor × §4.3 / §4.8 × `T_reproof` is called a default but is absent from configuration**

   It is described as a constant/default of 250 ms, but §4.8 lists no key and the validation rules do not cover it.

   **Fix:** Either declare it a non-configurable named constant everywhere or add a validated configuration key. The safety fix above should remove it from deliberate park decisions.

## Panel-resolution ledger

Actually resolved in the fold:

- Wrap-don’t-copy, audit coalescing dropped, and per-command audit retained.
- Decode thread has no drive reference; `BlockRead` removes motion by type.
- Exactly one SCSI data command in flight is coherent by ownership construction.
- Count recomputation before every issue; only the buffer is staged.
- GOOD + tripwire-RP-fail and impossible-residual rows are present.
- Recovered VALID nonzero residual and `29/07` are specified fail-closed in the read funnel.
- Per-mount identity verification and the multi-object physical leg are added.
- Decode→sender channel is byte-sized.
- `feed_gap_us` subtracts free-wait and park; cross-thread counters are atomic or post-join.
- R1’s naive blocking-send and zero-copy proposals were removed/replaced.
- Read-cadence uncertainty is now stated.
- R2 measurement gating was explicitly overruled by the owner, rather than silently ignored.
- SHA-NI and 4 KiB costs are made pre-measurement obligations.

Not actually resolved:

- Panel SCSI M3 ranged-read integrity: transport unification is resolved; the proof/release mechanism is not implementable with the specified handoff.
- Panel failure-mode error precedence: priority is stated, but the once-cell mechanism cannot enforce it.
- Panel residual/write-wide hardening: read handling is specified; cross-write-funnel behavior and compatibility are not.

No files were modified and no implementation tests were run, consistent with the read-only design-review charter.

---

## FINAL VERIFY (codex gpt-5.6-sol, 2026-07-11 11:41) — v0.4 after IN_BAND removal

**PASS — no blockers or majors. TIO-6 v0.4 meets the freeze bar; design FREEZES.**

IN_BAND genuinely deleted (3 majors dissolved). Proof-frontier implementable + correctly attributed (off-by-one prohibited+tested). Proof-only blocking benign/deadlock-free. Keepalive + RAM values present (tonic 0.14.5 exposes APIs; live builder intentionally pre-R2). RAM/residency scope sound — tmpfs spool ceiling-reserved-but-swappable, NO TIO-5 never-swap contradiction. Cycle arithmetic checks (~50.7 cyc/hr @ 8GiB/90-25/c=d/2; leg-3 = physical qual, not a defect). Error precedence fixed. Accepted decisions intact.

Two MINORS to fold at prompt-cut: (1) reference-configuration.md missing 4 reservoir/proof keys + spool-row post-R2 authority wording; (2) §3.2 channel-growth assertion — size at window-creation for effective max slab count, assert allocated ≤ capacity.
