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
